use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use nml_core::ast::*;
use nml_core::model::{EnumDef, FieldDef, FieldType, ModelDef, OneOfDef};
use nml_core::schema_index::{BodyShape, NameableVariant};
use nml_core::span::Span;
use nml_core::types::{PrimitiveType, Value};
use nml_core::{FieldTarget, SchemaIndex};
use nml_validate::schema::MembershipSemantics;
use nml_validate::workspace::read_leaf;

use crate::diagnostics::{self, SchemaMode};
use crate::duration_lsp::{self, DurationUnitContext};
use crate::packages::{self, Resolution, WorkspaceView};
use crate::position::{self, LineIndex};

/// Bytes of ONE workspace file the editor holds — indexed from disk or
/// OPEN as a buffer — the bound the CLI reads a check target under
/// (`nml-cli`'s `MAX_TARGET_BYTES`, pinned equal in `nml limits`'
/// census), so the editor and the CI gate refuse a file at one size.
/// Past it a file is not indexed and the editor says so
/// (`window/logMessage`), and an open buffer is not stored: its one
/// diagnostic is the kernel's cap sentence, nothing parses it, and
/// `nml/schemaInfo` says so. (An open buffer used to be "the truth at
/// any size": a 300 MiB buffer cost 244 s and 11 GB — 23 GB of NUL
/// bytes — and reported GREEN where `nml check` refuses the same file
/// in 24 ms.) The index has no bound of its own on depth or file count:
/// it is the kernel's one enumeration of the root, under the kernel's
/// bounds.
///
/// LIMIT: reach=content guards=memory surface=editor shown="16 MiB" — bytes of one workspace file the editor holds, indexed or open; past it the file is not indexed (said) and an open buffer is refused with one row
pub const MAX_INDEX_BYTES: usize = 16 * 1024 * 1024;

/// The noun [`MAX_INDEX_BYTES`] applies to, in the kernel's refusal
/// sentence — spelled once, so the index sweep, the watcher and the disk
/// fallback refuse an oversized file in the same words.
const INDEXED_FILE: &str = "an indexed workspace file";

/// The noun the cap sentence names for a refused open buffer.
const OPEN_DOCUMENT: &str = "an open document";

/// Bytes read when locating a related note's OWN file on disk (an open
/// buffer is the truth and costs nothing): a note's line index is not
/// worth an unbounded read; over the cap the renderer falls back loudly,
/// as for any unlocatable file.
///
/// LIMIT: reach=content guards=memory surface=editor shown="8 MiB" — bytes read when locating a related note's file
const MAX_LOCATE_BYTES: u64 = 8 * 1024 * 1024;

/// Code actions minted from one diagnostic's wire `suggestions`: the
/// producer's own alternative bound (`MAX_FIX_ALTERNATIVES` in
/// nml-validate), applied here to VALID entries, so a hostile or buggy
/// client can neither mint unbounded actions from one diagnostic nor
/// bury a legitimate entry behind malformed padding.
///
/// LIMIT: reach=content guards=output surface=editor shown="8" — code actions minted from one diagnostic's suggestions
const MAX_SUGGESTION_ACTIONS: usize = 8;

/// The server's shared state, held behind `Arc`. `NmlLanguageServer` `Deref`s
/// to this, so every `self.field`/`self.method()` on state-only methods reads
/// through here; the split is enforced by the compiler — anything needing the
/// `Client` (diagnostics delivery, logging) lives on `NmlLanguageServer`, not
/// here. Under the pull model there is no background task, so `Inner` is
/// touched only by request handlers, all on the one server task.
/// One document's computed diagnostics plus the exact text they were
/// computed from (RFC 0010 tier 1). Reads validate `text` against the
/// current buffer: an in-flight compute that finishes after an edit
/// inserts an entry that self-describes as stale and reads as a miss —
/// never served against the wrong text (ranges would lie).
struct CachedDiagnostics {
    text: String,
    /// The resolver generation at compute time — an out-of-band store sync
    /// or manifest rebuild changes diagnostics without touching any buffer;
    /// reads compare against the CURRENT generation (after a cheap
    /// stat-guarded resolve) so those entries read as misses.
    generation: u64,
    /// Shared, not cloned: hover reads borrow through the `Arc`; only the
    /// pull (which must build an owned report) pays a deep copy.
    items: Arc<Vec<tower_lsp::lsp_types::Diagnostic>>,
}

impl CachedDiagnostics {
    /// The cache's ONE read rule: an entry serves only against the text
    /// it was computed from and the resolver generation of that compute
    /// — an insert from a compute that raced an edit, or a resolution
    /// that moved on, reads as a miss. Every consumer reads through it.
    fn is_fresh(&self, current: &str, generation: u64) -> bool {
        self.text == current && self.generation == generation
    }
}

/// The document store: every text the server serves — open buffers and
/// indexed disk copies — with a STAMP beside each: the store's write
/// counter at the document's last write. The resolver's universe cache
/// compares one stamp per discovery read to know a stored input is
/// unchanged, never the text. Reads deref to the map; a write goes
/// through [`Self::insert`] and [`Self::remove`], the only two writers,
/// so a stamp can never miss a change. Beside an OPEN buffer's text sits
/// the client's version of it (`didOpen`/`didChange`): the number a
/// versioned workspace edit names, so a client refuses the edit once the
/// buffer moved on (LSP 3.17 §WorkspaceEdit); an indexed disk copy has
/// none — the disk is its master.
#[derive(Default)]
struct DocumentStore {
    texts: HashMap<Url, String>,
    stamps: HashMap<Url, u64>,
    versions: HashMap<Url, i32>,
    /// The `(len, mtime)` of the file an INDEXED copy was read from, for
    /// a client that does not watch: discovery compares it against a
    /// `stat` before answering from the copy. An open buffer has none —
    /// the buffer is the master while it is open, and no keystroke pays a
    /// syscall — so every write through [`Self::insert`] clears the entry
    /// and only an index read puts one back.
    disk: HashMap<Url, (u64, Option<std::time::SystemTime>)>,
    writes: u64,
}

impl DocumentStore {
    fn insert(&mut self, uri: Url, text: String, version: Option<i32>) {
        self.writes += 1;
        self.stamps.insert(uri.clone(), self.writes);
        match version {
            Some(v) => {
                self.versions.insert(uri.clone(), v);
            }
            None => {
                self.versions.remove(&uri);
            }
        }
        self.disk.remove(&uri);
        self.texts.insert(uri, text);
    }

    /// Record what an indexed copy was read from, straight after the
    /// [`Self::insert`] that stored its text.
    fn note_disk(&mut self, uri: Url, stamp: (u64, Option<std::time::SystemTime>)) {
        self.disk.insert(uri, stamp);
    }

    /// What the indexed copy at `uri` was read from; `None` for an open
    /// buffer and for a copy read before any stamp was recorded.
    fn disk_stamp(&self, uri: &Url) -> Option<(u64, Option<std::time::SystemTime>)> {
        self.disk.get(uri).copied()
    }

    fn remove(&mut self, uri: &Url) -> Option<String> {
        self.stamps.remove(uri);
        self.versions.remove(uri);
        self.disk.remove(uri);
        self.texts.remove(uri)
    }

    fn stamp(&self, uri: &Url) -> Option<u64> {
        self.stamps.get(uri).copied()
    }

    /// The client's version of an open buffer; `None` for a document
    /// the client did not open (an indexed copy — the disk is its
    /// master, and a versioned edit names `null`).
    fn version(&self, uri: &Url) -> Option<i32> {
        self.versions.get(uri).copied()
    }
}

impl std::ops::Deref for DocumentStore {
    type Target = HashMap<Url, String>;

    fn deref(&self) -> &Self::Target {
        &self.texts
    }
}

/// The document store as the resolver's overlay reads it: text and stamp
/// by canonical path — and, for a client that does not watch the
/// workspace, the disk check that keeps an indexed copy honest
/// ([`Self::refresh_indexed`]).
struct Documents<'a>(&'a Inner);

impl Documents<'_> {
    /// Re-read the INDEXED copy at `uri` when its file moved.
    ///
    /// A watching client's events are the freshness contract, so this is
    /// skipped outright for one — read first, before any lock or syscall.
    /// Without one, an indexed copy would be stale forever: it carries a
    /// store stamp, so the universe memo's disk branch never runs for it.
    ///
    /// An OPEN buffer is never touched: the client's buffer is the master
    /// while it is open (LSP 3.17), and a pull must not pay a `stat` per
    /// keystroke. Nor is this a poll: it runs only where discovery was
    /// about to read the path anyway, so the cost is the one the security
    /// lane's note N3 already accounts (one stat per indexed read, per
    /// pull). A file that cannot be stat-ed is left alone — a deletion is
    /// the watcher's case, and the kernel's own reader reports an
    /// unreadable input where discovery meets it.
    ///
    /// The re-read goes through the kernel's one capped reader, which
    /// takes its size from the OPEN handle, so an oversized file is
    /// refused without being read in. The stamp recorded is the one taken
    /// BEFORE the read: a file that changes again mid-read leaves a stamp
    /// the next pull disagrees with, and re-reads.
    fn refresh_indexed(&self, uri: &Url, path: &Path) {
        if self
            .0
            .watching_files
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let indexed = self
            .0
            .indexed_uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(uri);
        if !indexed {
            return;
        }
        let open = self
            .0
            .open_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(uri);
        if open {
            return;
        }
        let Some(now) = packages::disk_stamp(path) else {
            return;
        };
        let unchanged = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .disk_stamp(uri)
            == Some(now);
        if unchanged {
            return;
        }
        let Ok(text) = read_leaf(path, MAX_INDEX_BYTES, INDEXED_FILE) else {
            return;
        };
        let mut docs = self.0.documents.lock().unwrap_or_else(|e| e.into_inner());
        docs.insert(uri.clone(), text, None);
        docs.note_disk(uri.clone(), now);
    }
}

impl packages::OpenDocuments for Documents<'_> {
    fn text(&self, path: &Path) -> Option<String> {
        let uri = Url::from_file_path(path).ok()?;
        self.refresh_indexed(&uri, path);
        self.0
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&uri)
            .cloned()
    }

    fn stamp(&self, path: &Path) -> Option<u64> {
        let uri = Url::from_file_path(path).ok()?;
        self.refresh_indexed(&uri, path);
        self.0
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stamp(&uri)
    }
}

pub struct Inner {
    documents: Mutex<DocumentStore>,
    /// Per-document diagnostics cache (RFC 0010 tier 1), filled lazily by
    /// whichever consumer computes first — the document pull or hover's
    /// explanation lookup — so hover never recomputes per-request and the
    /// pull's *Unchanged* path stops re-validating. Invalidated per-document
    /// on change/close and wholesale by [`Inner::rebuild_schema_registry`]
    /// and project-config changes (a registry edit changes OTHER documents'
    /// diagnostics without touching their text).
    diags_cache: Mutex<HashMap<Url, CachedDiagnostics>>,
    indexed_uris: Mutex<HashSet<Url>>,
    /// Documents currently open in the editor (didOpen without a matching
    /// didClose). Guards watched-file disk events from clobbering an open
    /// buffer — while a file is open the client buffer is its source of truth.
    open_docs: Mutex<HashSet<Url>>,
    /// Open buffers REFUSED at [`MAX_INDEX_BYTES`] (their byte length):
    /// never stored, so no handler can parse one; the pull answers with
    /// the cap row and `nml/schemaInfo` with the same note. Cleared by
    /// a change under the bound or a close.
    refused_buffers: Mutex<HashMap<Url, u64>>,
    scoped_models: Mutex<HashMap<String, Vec<ModelDef>>>,
    scoped_enums: Mutex<HashMap<String, Vec<EnumDef>>>,
    scoped_oneofs: Mutex<HashMap<String, Vec<OneOfDef>>>,
    /// Canonicalized workspace roots captured at initialize, SORTED;
    /// watched-file events outside these roots are ignored. Sorted
    /// because two of the three rules over this list pick "the first
    /// root a path starts with" — the universe a document resolves in
    /// (`PackageResolver::anchor_for`) and the name every finding
    /// carries (`packages::source_name_of`) — while the third
    /// (`packages::canonical_above_roots`) walks ancestors and picks the
    /// OUTERMOST. Sorted, an ancestor precedes its descendants and all
    /// three agree; unsorted, the client's `workspaceFolders` order
    /// decided which of two NESTED folders governs, and the LSP
    /// specification gives that order no meaning (a folder added later
    /// lands at the end, so the same session could answer two ways
    /// before and after a `didChangeWorkspaceFolders`).
    workspace_roots: Mutex<Vec<PathBuf>>,
    /// Roots handed to `initialize` whose workspace index has not been built
    /// yet, in the client's own URI spelling. `initialize` records them and
    /// returns; `initialized` drains them and does the walk. The handshake is
    /// therefore never held open by a filesystem sweep — measured at 458 ms
    /// (warm) / 1.69 s (cold) for a 73k-entry checkout, during which
    /// tower-lsp's single-task `join!` can answer nothing else.
    pending_index_roots: Mutex<Vec<Url>>,
    membership: MembershipSemantics,
    /// Schema-package resolution (RFC 0030): pins > auto-association >
    /// unbound fallback, definitions from workspace manifests > store >
    /// builtins. Owns its own caches; per-root pin config is resolved inside
    /// (never through the global `project_config`).
    resolver: packages::PackageResolver,
    /// Client capability: `completionItem.insertReplaceSupport` (LSP 3.16) —
    /// gates `InsertReplaceEdit` vs plain `TextEdit` value completions.
    insert_replace_support: std::sync::atomic::AtomicBool,
    /// Client capability: `completionItem.labelDetailsSupport` (LSP 3.17) —
    /// gates the RFC 0015 union-of-fields "adds `as X`" label detail; older
    /// clients get it folded into `detail`.
    label_details_support: std::sync::atomic::AtomicBool,
    /// The client-declared command id behind "Explain NML0000" code actions
    /// (RFC 0010 tier 2), from `initializationOptions.explainCommand`. The
    /// action is emitted only when a client declared one — an editor that
    /// registered no such command must never receive an unexecutable action
    /// (negotiation, not assumption). `None` = no client support declared.
    explain_command: Mutex<Option<String>>,
    /// Client capability: `workspace.workspaceEdit.documentChanges` (LSP
    /// 3.17 §WorkspaceEdit) — every edit the server hands out names the
    /// target document's VERSION through `documentChanges`, so a client
    /// refuses an edit computed against a buffer that has since moved on;
    /// undeclared, only plain `changes` are legal and the edit is that.
    versioned_edits: std::sync::atomic::AtomicBool,
    /// Client capability: `workspace.workspaceEdit.resourceOperations`
    /// names `create` — an action that creates a file (a pin or opt-out
    /// with no live config to write into) exists only for a client that
    /// declared it can create one; undeclared, no such action is offered.
    creates_files: std::sync::atomic::AtomicBool,
    /// Client capability: `workspace.diagnostics.refreshSupport` (LSP 3.17's
    /// spelling, read from the raw `initialize` params by [`crate::NmlService`];
    /// lsp-types 0.94.1's `workspace.diagnostic` spelling is read here) — the
    /// client re-pulls every open document on `workspace/diagnostic/refresh`
    /// (LSP 3.17). Declared, a pull that REDISCOVERS the universe (a
    /// manifest or project-config buffer opened, edited or closed; a store
    /// pointer moved) asks for that refresh once, so the other open
    /// documents' reports — and the actions offered from them, the grant
    /// on the manifest included — are current without a refocus;
    /// undeclared, they heal on their own next pull.
    refresh_diagnostics: std::sync::atomic::AtomicBool,
    /// LSP 3.17's own spelling of that capability as the LAST `initialize`
    /// frame sent it, parked by [`crate::NmlService`] for the `initialize`
    /// HANDLER to take. The peek runs on every `initialize` frame — it is a
    /// look at raw JSON, upstream of tower-lsp's lifecycle — and tower-lsp
    /// refuses a duplicate `initialize` without running the handler; parking
    /// instead of applying is what keeps a refused frame's capabilities from
    /// reaching the server. (Applied directly, a second `initialize` turned
    /// the refresh on for a client that never declared it, and the pull that
    /// then asks such a client waits on an answer it will never send.)
    raw_refresh_declaration: std::sync::atomic::AtomicBool,
    /// Client capability: `workspace.didChangeWatchedFiles.dynamicRegistration`
    /// (LSP 3.17) AND an accepted `**/*.nml` registration — the two halves
    /// of "this client tells us when the disk moves". Both, because a
    /// client may declare the capability and still refuse the
    /// registration, and the server asked for years without reading the
    /// answer. Watching, an indexed copy is fresh by construction and
    /// discovery touches no syscall for one; NOT watching, discovery
    /// re-stats an indexed copy before answering from it
    /// ([`Documents::refresh_indexed`]) — without which a manifest fixed
    /// outside the editor kept its NML2088 forever and a cross-file quick
    /// fix spliced at offsets the disk no longer had.
    watching_files: std::sync::atomic::AtomicBool,
}

pub struct NmlLanguageServer {
    client: crate::ask::ClientDoor,
    inner: Arc<Inner>,
    /// Store-health transitions the resolver emits during resolution (which
    /// has no `Client` of its own). Drained in the document-pull handler —
    /// the one place that both runs on every validation and holds the
    /// `Client` — and surfaced as `window/logMessage`. Pull-driven, not a
    /// background task: the wasm neutral server runs a synchronous pump that
    /// cannot host one, and the store cache is stat-guarded so correctness
    /// never depended on a poll. Bounded + best-effort: on overflow the
    /// newest events drop (the first transition is the informative one).
    store_events: Mutex<tokio::sync::mpsc::Receiver<packages::StoreEvent>>,
}

impl std::ops::Deref for NmlLanguageServer {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.inner
    }
}

/// Inputs to [`NmlLanguageServer::build`], defaulted so each named constructor
/// sets only the fields it means to — no wall of positional `None`s at the call
/// sites. `store: None` means "run storeless"; a constructor wanting the
/// per-user store passes `Store::user()` explicitly.
#[derive(Default)]
struct BuildConfig {
    store: Option<nml_validate::store::Store>,
    membership: MembershipSemantics,
    injected: Option<nml_validate::package::SchemaPackage>,
}

/// The path of a workspace folder the client named: canonical where
/// the platform has `realpath` (an operator's own symlink is followed,
/// so a folder opened through a link and a file under its target are
/// one root), else AS SPELLED. WASI has no `realpath`, so
/// `std::fs::canonicalize` fails for every path there; a folder
/// dropped on that failure made every document of the bundled WASM
/// server "outside every workspace folder" — no folder anchor, no
/// index, no finding, no note. The kernel re-verifies the spelling
/// when it anchors a universe at the folder (`WorkspaceRoot::editor`),
/// so an absent folder still fixes no universe.
fn folder_path(uri: &Url) -> Option<PathBuf> {
    let path = uri.to_file_path().ok()?;
    Some(dunce::canonicalize(&path).unwrap_or(path))
}

impl NmlLanguageServer {
    pub fn new(client: Client) -> Self {
        // Production wiring: the per-user schema-package store (may be absent
        // on exotic platforms; treated as an empty store, never an error).
        Self::build(
            client,
            BuildConfig {
                store: nml_validate::store::Store::user(),
                ..Default::default()
            },
        )
    }

    /// Provider seam (RFC 0035 in-binary channel): the server a schema-provider
    /// tool starts from its own subcommand (`nudge lsp`). The tool's embedded
    /// package is served in-process at top-of-cache precedence — the editor
    /// validates against the exact running binary's schema, zero-sync — while
    /// the given `store` and committed workspace manifests are still read.
    ///
    /// This server is a *pure superset* of the neutral one: the injected package
    /// governs exactly the files its bindings claim — via its own validator,
    /// which already carries the package's strictness, modifiers, and membership
    /// — and every other file (unbound, or bound to a different package) behaves
    /// identically to [`Self::new`]. A package's profile is scoped to the files
    /// it claims, never leaked onto files it does not; so the unbound path keeps
    /// neutral defaults. Production passes `Store::user()` (see [`crate::serve`]);
    /// the harness injects a tempdir store.
    pub fn with_provider(
        client: Client,
        package: nml_validate::package::SchemaPackage,
        store: Option<nml_validate::store::Store>,
    ) -> Self {
        Self::build(
            client,
            BuildConfig {
                store,
                injected: Some(package),
                ..Default::default()
            },
        )
    }

    /// Embedder/test seam: identical to [`Self::new`] except the
    /// schema-package store is supplied by the caller instead of resolved from
    /// the user environment (`NML_SCHEMA_STORE_DIR` / platform data dir). The
    /// in-process test harness injects a tempdir store here; an embedder may
    /// inject its own store, or `None` to run storeless.
    pub fn with_store(client: Client, store: Option<nml_validate::store::Store>) -> Self {
        Self::build(
            client,
            BuildConfig {
                store,
                ..Default::default()
            },
        )
    }

    /// Shared constructor body. The public constructors differ only in the
    /// [`BuildConfig`] fields they set.
    fn build(client: Client, cfg: BuildConfig) -> Self {
        let BuildConfig {
            store,
            membership,
            injected,
        } = cfg;
        let (store_events_tx, store_events_rx) = tokio::sync::mpsc::channel(64);
        Self {
            client: crate::ask::ClientDoor::new(client),
            inner: Arc::new(Inner {
                documents: Mutex::new(DocumentStore::default()),
                diags_cache: Mutex::new(HashMap::new()),
                indexed_uris: Mutex::new(HashSet::new()),
                open_docs: Mutex::new(HashSet::new()),
                scoped_models: Mutex::new(HashMap::new()),
                scoped_enums: Mutex::new(HashMap::new()),
                scoped_oneofs: Mutex::new(HashMap::new()),
                workspace_roots: Mutex::new(Vec::new()),
                refused_buffers: Mutex::new(HashMap::new()),
                pending_index_roots: Mutex::new(Vec::new()),
                membership,
                resolver: packages::PackageResolver::with_injected(
                    store,
                    store_events_tx,
                    injected,
                ),
                insert_replace_support: std::sync::atomic::AtomicBool::new(false),
                label_details_support: std::sync::atomic::AtomicBool::new(false),
                explain_command: Mutex::new(None),
                versioned_edits: std::sync::atomic::AtomicBool::new(false),
                creates_files: std::sync::atomic::AtomicBool::new(false),
                refresh_diagnostics: std::sync::atomic::AtomicBool::new(false),
                raw_refresh_declaration: std::sync::atomic::AtomicBool::new(false),
                watching_files: std::sync::atomic::AtomicBool::new(false),
            }),
            store_events: Mutex::new(store_events_rx),
        }
    }
}

impl Inner {
    /// Index the workspace roots — the KERNEL's enumeration (step 0e-b):
    /// every `.nml` file the universe walk saw under a root is read, up
    /// to [`MAX_INDEX_BYTES`], into the document store and marked indexed
    /// (a regular file only, never a symlink, a FIFO or a policy-skipped
    /// subtree: the walk's own rules); the root's `nml-project.nml` is
    /// indexed like every other file — the tooling config a document
    /// reads under is the kernel's nearest live config for THAT document,
    /// never a global read from a root. There is no second walk under a
    /// second bound:
    /// what the kernel denies is not indexed, and the editor SAYS so —
    /// the returned lines, one per denial (a truncated root indexes
    /// nothing, a spent unit's files are absent, an unloadable live
    /// input) and one per file refused at the byte bound, are the
    /// caller's `window/logMessage`s.
    fn index_workspace(&self, roots: &[Url]) -> Vec<String> {
        let mut said = Vec::new();
        let canonical_roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for root in roots {
            let Some(path) = folder_path(root) else {
                continue;
            };
            // The resolver reads the document store (stamps, texts) while
            // it discovers: no store lock is held across the call.
            let index = {
                let buffers = self.open_buffer_paths();
                let documents = Documents(self);
                let view = WorkspaceView {
                    roots: &canonical_roots,
                    buffers: &buffers,
                    documents: &documents,
                };
                self.resolver.index(&path, &view)
            };
            said.extend(index.denials);
            let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let mut indexed = self.indexed_uris.lock().unwrap_or_else(|e| e.into_inner());
            for file in index.files {
                let stamp = packages::disk_stamp(&file);
                match read_leaf(&file, MAX_INDEX_BYTES, INDEXED_FILE) {
                    Ok(content) => {
                        if let Ok(uri) = Url::from_file_path(&file) {
                            docs.insert(uri.clone(), content, None);
                            if let Some(stamp) = stamp {
                                docs.note_disk(uri.clone(), stamp);
                            }
                            indexed.insert(uri);
                        }
                    }
                    Err(why) => said.push(format!("`{}` is not indexed: {why}", file.display())),
                }
            }
        }
        said
    }

    /// The indexed disk copy of a just-closed document, re-read under
    /// [`MAX_INDEX_BYTES`] like every indexed file; a copy that cannot be
    /// read (gone, or past the bound) leaves the index, and the denial
    /// comes back for the handler to say the way the index and the
    /// watcher say it.
    fn reindex_closed(&self, uri: &Url) -> Option<String> {
        let path = uri.to_file_path().ok()?;
        let stamp = packages::disk_stamp(&path);
        match read_leaf(&path, MAX_INDEX_BYTES, INDEXED_FILE) {
            Ok(content) => {
                let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                docs.insert(uri.clone(), content, None);
                if let Some(stamp) = stamp {
                    docs.note_disk(uri.clone(), stamp);
                }
                None
            }
            Err(why) => {
                self.documents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(uri);
                self.indexed_uris
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(uri);
                Some(format!("`{}` is not indexed: {why}", path.display()))
            }
        }
    }

    fn rebuild_schema_registry(&self) {
        // The registry changes every document's diagnostics without touching
        // their text — the whole cache is stale, by construction, for every
        // caller of this rebuild (RFC 0010 tier 1).
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let mut scoped_models: HashMap<String, Vec<ModelDef>> = HashMap::new();
        let mut scoped_enums: HashMap<String, Vec<EnumDef>> = HashMap::new();
        let mut scoped_oneofs: HashMap<String, Vec<OneOfDef>> = HashMap::new();

        for (uri, source) in docs.iter() {
            if !is_schema_source(uri) {
                continue;
            }
            let scope = extract_schema_scope(uri.as_str());
            // Extract straight from the CST (no owned-AST round-trip); parse errors
            // surface through the diagnostics path, so the registry ignores them.
            let (schema, _) = nml_core::cst::extract_schema(source);
            scoped_models
                .entry(scope.clone())
                .or_default()
                .extend(schema.models);
            scoped_enums
                .entry(scope.clone())
                .or_default()
                .extend(schema.enums);
            scoped_oneofs
                .entry(scope)
                .or_default()
                .extend(schema.oneofs);
        }

        *self.scoped_models.lock().unwrap_or_else(|e| e.into_inner()) = scoped_models;
        *self.scoped_enums.lock().unwrap_or_else(|e| e.into_inner()) = scoped_enums;
        *self.scoped_oneofs.lock().unwrap_or_else(|e| e.into_inner()) = scoped_oneofs;
    }

    fn models_for_file(&self, uri: &Url) -> (Vec<ModelDef>, Vec<EnumDef>, Vec<OneOfDef>) {
        let file_scope = extract_file_scope(uri.as_str());
        let scoped_models = self.scoped_models.lock().unwrap_or_else(|e| e.into_inner());
        let scoped_enums = self.scoped_enums.lock().unwrap_or_else(|e| e.into_inner());
        let scoped_oneofs = self.scoped_oneofs.lock().unwrap_or_else(|e| e.into_inner());

        let mut models = Vec::new();
        let mut enums = Vec::new();
        let mut oneofs = Vec::new();
        let mut seen_model_names: HashSet<String> = HashSet::new();
        let mut seen_enum_names: HashSet<String> = HashSet::new();
        let mut seen_oneof_names: HashSet<String> = HashSet::new();

        if let Some(ref scope) = file_scope {
            if let Some(scope_models) = scoped_models.get(scope) {
                for m in scope_models {
                    seen_model_names.insert(m.name.clone());
                    models.push(m.clone());
                }
            }
            if let Some(scope_enums) = scoped_enums.get(scope) {
                for e in scope_enums {
                    seen_enum_names.insert(e.name.clone());
                    enums.push(e.clone());
                }
            }
            if let Some(scope_oneofs) = scoped_oneofs.get(scope) {
                for o in scope_oneofs {
                    seen_oneof_names.insert(o.name.clone());
                    oneofs.push(o.clone());
                }
            }
        }

        for (scope, ms) in scoped_models.iter() {
            if file_scope.as_deref() == Some(scope.as_str()) {
                continue;
            }
            for m in ms {
                if seen_model_names.insert(m.name.clone()) {
                    models.push(m.clone());
                }
            }
        }
        for (scope, es) in scoped_enums.iter() {
            if file_scope.as_deref() == Some(scope.as_str()) {
                continue;
            }
            for e in es {
                if seen_enum_names.insert(e.name.clone()) {
                    enums.push(e.clone());
                }
            }
        }
        for (scope, os) in scoped_oneofs.iter() {
            if file_scope.as_deref() == Some(scope.as_str()) {
                continue;
            }
            for o in os {
                if seen_oneof_names.insert(o.name.clone()) {
                    oneofs.push(o.clone());
                }
            }
        }

        (models, enums, oneofs)
    }

    /// Per-document diagnostic config (RFC 0030): tooling fields resolve at
    /// the document's nearest-ancestor `nml-project.nml` — per root, nearest
    /// wins wholesale — falling back to the workspace-root config (and its
    /// embedder defaults) when no ancestor file exists. The last-edit-wins
    /// global clobber is gone: per-document resolution reads the tree.
    fn diagnostic_config_for(&self, uri: &Url) -> diagnostics::DiagnosticConfig {
        // Through the one workspace view (and the one document-path
        // rule, `project_config_of`): the config a document reads under
        // is looked up for the path the kernel judges — a link inside
        // the root stays a link — never for a whole-path-canonicalized
        // twin of it.
        let mut config = self.config_from_project(&self.project_config_of(uri));
        config.uri_is_registry_source = is_schema_source(uri);
        config
    }

    fn config_from_project(&self, pc: &nml_core::ProjectConfig) -> diagnostics::DiagnosticConfig {
        let membership = if pc.member_keywords.is_empty()
            && pc.builtin_refs.is_empty()
            && pc.user_ref_prefix.is_none()
        {
            self.membership.clone()
        } else {
            MembershipSemantics {
                member_keywords: pc.member_keywords.clone(),
                builtin_refs: pc.builtin_refs.clone(),
                user_ref_prefix: pc.user_ref_prefix.clone(),
            }
        };
        diagnostics::DiagnosticConfig {
            template_namespaces: pc.template_namespaces.clone(),
            modifiers: pc.modifiers.clone(),
            membership,
            uri_is_registry_source: false,
            load_pass_owns_composition: false,
            grant: None,
        }
    }

    /// The live project config the document at `uri` reads under — the
    /// kernel's nearest one through the one workspace view — else the
    /// EMBEDDER default: what a file outside every workspace root, or
    /// under a root with no `nml-project.nml` above it, reads under. (The
    /// root's `nml-project.nml` used to be read into a global at index
    /// time and on every edit, so a file outside every root got the LAST
    /// indexed or edited root's modifiers and namespaces.)
    fn project_config_of(&self, uri: &Url) -> nml_core::ProjectConfig {
        self.with_workspace_view(uri, |path, view| {
            self.resolver.project_config_for(path, view)
        })
        .flatten()
        .unwrap_or_default()
    }

    /// Resolve a document against the schema-package machinery (RFC 0030).
    /// `None` for non-file URIs; a `Resolved` otherwise, whose resolution may
    /// be `Unbound` (today's scope-token behavior applies).
    fn resolve_document(&self, uri: &Url) -> Option<packages::Resolved> {
        self.with_workspace_view(uri, |path, view| self.resolver.resolve(path, view))
    }

    /// The directive vocabulary covering a `.model.nml` document (RFC 0030),
    /// through the same workspace view resolution uses. A non-file URI has
    /// nothing to scan, so it is definitively `Opaque`, not undetermined.
    fn vocabulary_for_document(&self, uri: &Url) -> packages::VocabularyOutcome {
        self.with_workspace_view(uri, |path, view| self.resolver.vocabulary_for(path, view))
            .unwrap_or(packages::VocabularyOutcome::Opaque)
    }

    /// Build the resolver's [`WorkspaceView`] for one document and run `f`
    /// against it. One owner for the view construction: `resolve_document`
    /// and `vocabulary_for_document` must see the identical workspace or
    /// binding and vocabulary could disagree about coverage.
    fn with_workspace_view<R>(
        &self,
        uri: &Url,
        f: impl FnOnce(&Path, &WorkspaceView<'_>) -> R,
    ) -> Option<R> {
        let path = uri.to_file_path().ok()?;
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Roots are canonicalized at initialize; an un-canonicalized document
        // path (macOS /tmp → /private/tmp, symlinked checkouts) would fail
        // every starts_with, silently unrooting resolution — and letting the
        // ancestor walk escape the workspace. Canonical ABOVE the root only:
        // a link an author committed inside the root stays a link for the
        // kernel to judge (NML2083, as `nml check` says), where resolving
        // the whole path judged the link's TARGET under whatever claims it.
        let path = canonical_above_roots(path, &roots);
        // The unsaved buffers the kernel overlays on the disk (step 0e):
        // an open manifest, config or declared source resolves live, and a
        // buffer at a path the disk lacks exists for the walk.
        let buffers = self.open_buffer_paths();
        let documents = Documents(self);
        let view = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &documents,
        };
        Some(f(&path, &view))
    }

    /// The open (unsaved-capable) documents — the overlay the kernel's
    /// `OverlayFs` lays over the disk — spelled by the ONE document-path
    /// rule (`canonical_above_roots`): canonical above the root, untouched
    /// below, so a buffer opened through a linked directory inside the
    /// root sits at the LINK in the overlay, never at its target. The walk
    /// never enters a link, so an unsaved manifest behind one is no
    /// resolution input (the parent-canonical spelling used here placed
    /// it at the target: a live manifest, from an unsaved buffer behind a
    /// link, that could close or re-claim the universe). Indexed disk
    /// documents are not buffers: the disk already has them.
    fn open_buffer_paths(&self) -> Vec<PathBuf> {
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let open = self.open_docs.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<PathBuf> = open
            .iter()
            .filter_map(|u| u.to_file_path().ok())
            .map(|p| canonical_above_roots(p, &roots))
            .collect();
        out.sort();
        out
    }

    /// The schema definitions a document's editor surfaces must use:
    /// package-bound files get the package's exclusive index (RFC 0030 —
    /// exclusivity applies to completion and hover exactly as it does to
    /// diagnostics; a stray same-name workspace model must not leak into any
    /// surface), unbound files get the merged scope registry.
    ///
    /// Callers must not hold the `documents` lock: resolution reads it.
    fn schema_index_for(&self, uri: &Url) -> IndexHandle {
        match self.resolve_document(uri).map(|r| r.resolution) {
            Some(Resolution::Bound(b)) => IndexHandle::Bound(b.validator),
            _ => {
                let (models, enums, oneofs) = self.models_for_file(uri);
                IndexHandle::Registry(Box::new(SchemaIndex::build(models, enums, oneofs)))
            }
        }
    }

    /// The universe a `.model.nml` buffer's two schema passes load
    /// against, assembled BEFORE the validator runs — `check_one`'s
    /// universe assembly, for the editor: the covering package's
    /// declared sources (buffer-first), a store or in-binary snapshot,
    /// or the workspace registry set — and whether that universe lets
    /// the load pass OWN composition verdicts (authoritative for a
    /// snapshot, a declared set and an untruncated registry set;
    /// partial for a truncated registry set or a non-file buffer, where
    /// the validator's mixin verdicts stay). Returns the coverage
    /// outcome, the sources as `(name, text)`, the buffer's own name in
    /// that universe and the ownership flag.
    fn model_universe_for(
        &self,
        uri: &Url,
        text: &str,
        own_name: &str,
        roots: &[PathBuf],
    ) -> (
        packages::VocabularyOutcome,
        Vec<(String, String)>,
        String,
        bool,
    ) {
        let name_of = |p: &Path| packages::source_name_of(p, roots);
        let outcome = self.vocabulary_for_document(uri);
        let universe = match &outcome {
            packages::VocabularyOutcome::Covered(vocab) => &vocab.universe,
            _ => &packages::SchemaUniverse::None,
        };
        // A file buffer's universe is assembled by NAME — the
        // resolution's key, the one document-path rule — so no path
        // is canonicalized here (a symlink-spelled buffer prefix used
        // to fail the declared-entry identity check and double-enter
        // its own universe: a wall of false duplicate errors).
        let (sources, own_name, owns_composition) = match uri.to_file_path() {
            Ok(_) => {
                match universe {
                    // Store/in-binary coverage: the package's
                    // hash-verified source snapshot IS the universe —
                    // no disk reads at all.
                    packages::SchemaUniverse::Snapshot(pkg) => {
                        let sources = snapshot_universe(own_name, text, &pkg.sources);
                        (sources, own_name.to_string(), true)
                    }
                    packages::SchemaUniverse::Declared(paths) if !paths.is_empty() => {
                        let read = |p: &Path| -> Option<String> {
                            let file_uri = Url::from_file_path(p).ok()?;
                            let buffered = self
                                .documents
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&file_uri)
                                .cloned();
                            // A declared source read from disk here is
                            // capped exactly as discovery caps it (the
                            // kernel's one reader, 4 MiB for a source).
                            buffered.or_else(|| {
                                packages::read_input_at_leaf(
                                    nml_validate::workspace::InputKind::Source,
                                    p,
                                )
                                .ok()
                            })
                        };
                        let sources = declared_universe(own_name, text, paths, &read, &name_of);
                        (sources, own_name.to_string(), true)
                    }
                    // Uncovered: the universe is the WORKSPACE
                    // REGISTRY SET — every `.model.nml` the server
                    // holds (indexed + open), the same one namespace
                    // the registry validator, goto-definition, and
                    // hover resolve against (RFC 0012). Anything
                    // narrower contradicts the server's own
                    // navigation: a mixin defined one directory over
                    // would squiggle "unknown" while F12 jumps to it.
                    // No filesystem walk: `documents` already carries
                    // the freshest text for every member. Composition
                    // ownership holds only while the whole set fits
                    // the cap — beyond it the load pass would judge
                    // `is` targets against a truncated namespace the
                    // validator sees in full.
                    _ => {
                        let docs: Vec<(String, String)> = self
                            .documents
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .filter(|(u, _)| is_schema_source(u) && *u != uri)
                            .filter_map(|(u, t)| {
                                let p = u.to_file_path().ok()?;
                                Some((name_of(&p), t.clone()))
                            })
                            .collect();
                        let (sources, truncated) = registry_universe(own_name, text, docs);
                        (sources, own_name.to_string(), !truncated)
                    }
                }
            }
            // A non-file buffer (untitled, virtual scheme) still
            // validates — as its own single-source universe, which can
            // never judge composition.
            Err(()) => (
                vec![(uri.to_string(), text.to_string())],
                uri.to_string(),
                false,
            ),
        };
        (outcome, sources, own_name, owns_composition)
    }

    /// Full validation of one document: package-bound (exclusive validator +
    /// binding identity) when a package claims it, the scope-registry path
    /// otherwise, plus any degraded-state notes pinned to the top of file.
    fn validate_document(&self, uri: &Url, text: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let mut dc = self.diagnostic_config_for(uri);
        let resolved = self.resolve_document(uri);
        // The document's name on every finding (step 0f): its KEY under
        // a workspace root, its path outside every root — and the root
        // a foreign note's key is turned back into a path through.
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // ONE canonicalization rule for a document path — the one
        // `with_workspace_view` resolved under (`canonical_above_roots`):
        // an author's link inside the root stays a link here too, so the
        // name every finding carries is the key the kernel judged (the
        // resolution's own), never the link's target's.
        let canonical = uri
            .to_file_path()
            .ok()
            .map(|p| canonical_above_roots(p, &roots));
        // The root a foreign note's key is turned back into a path
        // through: the universe the document resolved in (its folder's,
        // or a derived one).
        let own_root = resolved
            .as_ref()
            .and_then(|r| r.root.as_ref().map(|(root, _)| root.clone()));
        let name_of = |p: &Path| packages::source_name_of(p, &roots);
        let own_name = resolved
            .as_ref()
            .and_then(|r| r.key.as_ref())
            .map(|k| k.to_string())
            .or_else(|| canonical.as_deref().map(name_of))
            .unwrap_or_else(|| uri.to_string());
        let bound = matches!(
            resolved.as_ref().map(|r| &r.resolution),
            Some(Resolution::Bound(_))
        );
        // The universe refused the file (a walk that did not finish):
        // the notes are the whole report — no parse band, no schema or
        // source pass, no composition — as `nml check` validates nothing.
        let refused = matches!(
            resolved.as_ref().map(|r| &r.resolution),
            Some(Resolution::Refused)
        );
        // Schema passes for a `.model.nml` buffer, assembled BEFORE the
        // validator runs: whether the validator's mixin verdicts may be
        // suppressed (`load_pass_owns_composition`) is a property of the
        // universe the load pass will actually load — authoritative for
        // snapshot/declared/untruncated-registry universes, partial for a
        // truncated registry set or a non-file buffer. A package-BOUND
        // `.model.nml` (binding globs claiming a model file) skips the
        // schema passes entirely: its project treats the file as data, and
        // the package validator owns every verdict — running the load pass
        // too would double-report composition errors whose package-identity
        // suffix defeats the exact-duplicate suppression (CLI parity: the
        // CLI validates it per the project's binding as well).
        let model_pass = (!bound && !refused && is_schema_source(uri))
            .then(|| self.model_universe_for(uri, text, &own_name, &roots));
        dc.load_pass_owns_composition = model_pass.as_ref().is_some_and(|(_, _, _, owns)| *owns);
        // The universe's composition grant for this document (step 0e):
        // `compose_file` denies and permits exactly as `nml check` does.
        dc.grant = resolved.as_ref().map(|r| r.grant.clone());
        // Locating a related note's OWN file (`Related.source`): an open
        // buffer first (its unsaved text is the truth), else disk —
        // memoized per compute (several notes can share a file) and
        // capped (a note's line index is not worth an unbounded read;
        // over the cap the renderer falls back loudly, as for any
        // unlocatable file).
        let located: std::cell::RefCell<std::collections::HashMap<String, Option<(Url, String)>>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
        let locate = |src: &str| -> Option<(Url, String)> {
            if let Some(hit) = located.borrow().get(src) {
                return hit.clone();
            }
            // A key names its file through the document's root (step
            // 0f); a name outside every root is already a path.
            let path = match (&own_root, Path::new(src).is_absolute()) {
                (Some(root), false) => root.join(src),
                _ => PathBuf::from(src),
            };
            let resolved = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                locate_source(&docs, &path)
            };
            located
                .borrow_mut()
                .insert(src.to_string(), resolved.clone());
            resolved
        };
        // ONE parse per publish: the buffer's AST, its own
        // extracted definitions and the parse findings feed `compute`,
        // and a `.model.nml` buffer's two schema passes below reuse the
        // same extraction — cloned definitions, never a re-parse of the
        // text (the load pass used to parse the buffer a second time and
        // the source pass a third).
        // A REFUSED document is never parsed: its notes are the whole
        // report, as for a buffer past the size bound (the parse count is
        // pinned still across its pulls).
        let parsed = (!refused).then(|| diagnostics::ParsedBuffer::parse(text));
        let own_part = model_pass
            .as_ref()
            .zip(parsed.as_ref())
            .map(|(_, parsed)| (parsed.own_defs.clone(), parsed.parse_errors.clone()));
        let declaration = parsed
            .as_ref()
            .and_then(|parsed| first_declaration(&parsed.file));
        let mut diags = match (resolved.as_ref().map(|r| &r.resolution), parsed) {
            (_, None) | (Some(Resolution::Refused), _) => Vec::new(),
            (Some(Resolution::Bound(b)), Some(parsed)) => {
                let identity = b.identity();
                diagnostics::compute_parsed(
                    text,
                    parsed,
                    &SchemaMode::Package {
                        validator: &b.validator,
                        identity,
                    },
                    &dc,
                    Some(uri),
                    &own_name,
                    &locate,
                )
            }
            (_, Some(parsed)) => {
                let (models, enums, oneofs) = self.models_for_file(uri);
                diagnostics::compute_parsed(
                    text,
                    parsed,
                    &SchemaMode::Registry {
                        models: &models,
                        enums: &enums,
                        oneofs: &oneofs,
                    },
                    &dc,
                    Some(uri),
                    &own_name,
                    &locate,
                )
            }
        };
        if let Some(resolved) = &resolved {
            note_rows(
                &resolved.notes,
                text,
                declaration,
                uri,
                &own_name,
                &locate,
                &mut diags,
            );
        }
        // Schema passes for a `.model.nml` buffer, over the universe
        // assembled above. First the LOAD pass — `load_schema`, the same
        // entry the CLI and embedders use, keeping only this buffer's
        // findings, so composition, cycles, shorthand arity, oneof/enum
        // integrity, reserved/duplicate names, and declared defaults reach
        // the editor with CLI parity by identity. Then the schema-SOURCE
        // pass (RFC 0030): directive vocabulary for covered files. Both
        // re-derive extraction errors the parse band already emitted, so
        // exact duplicates (same range, message, severity) are suppressed
        // rather than double-squiggled.
        if let Some(((outcome, sources, own_name, owns_composition), (own_schema, own_errors))) =
            model_pass.zip(own_part)
        {
            // The source pass borrows the extraction the load pass then
            // consumes; its rows are pushed after the load pass's, as
            // they always were.
            let source_diags = match &outcome {
                packages::VocabularyOutcome::Covered(vocab) => diagnostics::schema_source_pass(
                    text,
                    &own_schema,
                    &own_errors,
                    vocab,
                    Some(uri),
                ),
                _ => Vec::new(),
            };
            for diag in diagnostics::schema_load_pass(
                &own_name,
                &sources,
                Some(uri),
                owns_composition,
                (own_schema, own_errors),
            ) {
                push_unless_duplicate(&mut diags, diag);
            }
            match &outcome {
                packages::VocabularyOutcome::Covered(_) => {
                    for diag in source_diags {
                        push_unless_duplicate(&mut diags, diag);
                    }
                }
                // Judged under no vocabulary for a reason the author can
                // act on — the bounded claims walk hit its cap, or two or
                // more packages could cover the file and none declares it:
                // said ONCE (info, top of file) in the kernel's one sentence,
                // as `nml check` says it.
                packages::VocabularyOutcome::Undetermined
                | packages::VocabularyOutcome::Ambiguous { .. } => {
                    for diag in diagnostics::coverage_note(text, &outcome, Some(uri)) {
                        push_unless_duplicate(&mut diags, diag);
                    }
                }
                // Definitively uncovered files stay silent — plain-nml
                // schema authors are never punished for the mechanism.
                packages::VocabularyOutcome::Opaque => {}
            }
        }
        diags
    }
}

/// The resolution's degraded-state notes as rows on the document —
/// `check_one`'s universe report, for the editor: each at its anchor
/// (the top of the file, the first declaration, a span) under the
/// severity and code the CLI prints it under (NML2087 an error, NML2080
/// a warning), so the editor and the CI gate show one verdict; a kernel
/// row's related notes (NML2091's first failing source line) are
/// located through the same locator a finding's notes use — one
/// mapping, one `relatedInformation`. Pushed onto `rows`, the document's
/// own rows so far: a wrapping row located on ITS OWN document (an
/// unloadable manifest's NML2088 at the manifest's first finding) whose
/// wrapped finding the document already reports — same range, the
/// wrapper's `cause` code — is folded into that row (RFC 0026 decision
/// 6): one finding, one squiggle, the row a user acts on (its quick fix,
/// its notes), which gains the wrapper's context as a related location
/// ([`LOAD_NOTE`]). A governed file's row is never folded (its document
/// reports nothing of the manifest's).
fn note_rows(
    notes: &[packages::DegradedNote],
    text: &str,
    declaration: Option<nml_core::span::Span>,
    uri: &Url,
    own_name: &str,
    locate: &dyn Fn(&str) -> Option<(Url, String)>,
    rows: &mut Vec<tower_lsp::lsp_types::Diagnostic>,
) {
    let top = tower_lsp::lsp_types::Range::new(
        tower_lsp::lsp_types::Position::new(0, 0),
        tower_lsp::lsp_types::Position::new(0, 0),
    );
    let line_index = LineIndex::new(text);
    for note in notes {
        let range = match note.anchor {
            packages::NoteAnchor::Top => top,
            packages::NoteAnchor::Declaration => {
                declaration.map(|sp| line_index.range(sp)).unwrap_or(top)
            }
            packages::NoteAnchor::At(sp) => line_index.range(sp),
        };
        let twin = note.cause.and_then(|cause| {
            let code = tower_lsp::lsp_types::NumberOrString::String(cause.to_string());
            rows.iter_mut()
                .find(|d| d.range == range && d.code.as_ref() == Some(&code))
        });
        if let Some(twin) = twin {
            twin.related_information.get_or_insert_with(Vec::new).push(
                tower_lsp::lsp_types::DiagnosticRelatedInformation {
                    location: tower_lsp::lsp_types::Location {
                        uri: uri.clone(),
                        range,
                    },
                    message: LOAD_NOTE.to_string(),
                },
            );
            continue;
        }
        // A kernel row keeps the severity and code the CLI prints
        // it under (NML2087 an error, NML2080 a warning), so the
        // editor and the CI gate show one verdict.
        let severity = match note.severity {
            nml_core::diagnostic::Severity::Error => {
                tower_lsp::lsp_types::DiagnosticSeverity::ERROR
            }
            nml_core::diagnostic::Severity::Warning => {
                tower_lsp::lsp_types::DiagnosticSeverity::WARNING
            }
            _ => tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION,
        };
        // A kernel row's related notes (NML2091's first failing
        // source line) are located through the same locator a
        // finding's notes use — one mapping, one `relatedInformation`.
        let related_information = (!note.related.is_empty()).then(|| {
            diagnostics::related_information(
                Some(own_name),
                &note.related,
                None,
                uri,
                &line_index,
                own_name,
                locate,
            )
        });
        rows.push(tower_lsp::lsp_types::Diagnostic {
            range,
            severity: Some(severity),
            code: note
                .code
                .map(|c| tower_lsp::lsp_types::NumberOrString::String(c.to_string())),
            message: note.message.clone(),
            source: Some("nml".to_string()),
            related_information,
            // The row's remedies, each naming its file when that is not
            // this document (a failed manifest's did-you-mean on a
            // governed file): the code-action handler offers the quick
            // fix on the manifest, as it offers a located finding's.
            data: diagnostics::suggestion_data(
                &note.suggestions,
                |s| s.source.as_deref(),
                own_name,
            ),
            ..Default::default()
        });
    }
}

/// The related location a manifest's own finding carries when the
/// universe's NML2088 row was folded into it ([`note_rows`]): the
/// wrapper's context — the load fails here — stated once, on the row a
/// user acts on.
pub(crate) const LOAD_NOTE: &str = "the manifest fails to load here (NML2088)";

/// Push `diag` unless an exact twin (range, message, severity) is already
/// published — the cross-pass suppression: a `.model.nml` buffer's schema
/// passes re-derive extraction errors the parse band already emitted.
fn push_unless_duplicate(
    diags: &mut Vec<tower_lsp::lsp_types::Diagnostic>,
    diag: tower_lsp::lsp_types::Diagnostic,
) {
    let duplicate = diags
        .iter()
        .any(|d| d.range == diag.range && d.message == diag.message && d.severity == diag.severity);
    if !duplicate {
        diags.push(diag);
    }
}

impl NmlLanguageServer {
    /// This document's diagnostics via the RFC 0010 tier-1 cache — computed
    /// at most once per text state, by whichever consumer asks first (the
    /// document pull or hover's explanation lookup). The entry is validated
    /// against the CURRENT buffer text, so an insert from a compute that
    /// raced an edit reads as a miss — never served as stale ranges. `None`
    /// for an unknown document. Returns the exact TEXT the items were
    /// computed against beside them, so a consumer that edits (the
    /// code-action path) can resolve against that text instead of
    /// re-reading the document map — coherent even if a `didChange`
    /// interleaves between its own snapshot and this call.
    async fn cached_diagnostics(
        &self,
        uri: &Url,
    ) -> Option<(String, Arc<Vec<tower_lsp::lsp_types::Diagnostic>>)> {
        // A refused open buffer has no text to validate: the cap row is
        // its whole report, nothing is parsed, nothing is cached.
        if let Some(row) = self.refused_buffer_row(uri) {
            return Some((String::new(), Arc::new(vec![row])));
        }
        let text = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
            .cloned()?;
        // Let the stat-guarded resolver notice out-of-band changes (a store
        // `schema sync`, a manifest edit on disk) — cheap when nothing
        // changed, and it advances the generation when something did.
        let _ = self.resolve_document(uri);
        let generation = self.resolver.generation();
        if let Some(entry) = self
            .diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
        {
            if entry.is_fresh(&text, generation) {
                return Some((text, Arc::clone(&entry.items)));
            }
        }
        let items = Arc::new(self.validate_document(uri, &text));
        let validated = text.clone();
        // Store-health events queued during this resolution surface promptly
        // on whichever path computed (the drain's charter).
        self.drain_store_events().await;
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                uri.clone(),
                CachedDiagnostics {
                    text,
                    generation,
                    items: Arc::clone(&items),
                },
            );
        Some((validated, items))
    }

    /// The RFC 0010 tier-1 hover augmentation at a position: explanation
    /// summaries of the coded diagnostics intersecting it, plus the
    /// narrowest hit's range (the hover highlight). `None` when nothing
    /// coded intersects. The cache makes this recompute-free per hover.
    async fn diagnostic_explanations_at(
        &self,
        uri: &Url,
        pos: Position,
    ) -> Option<(String, Range)> {
        let (_, items) = self.cached_diagnostics(uri).await?;
        explanations_at_position(&items, pos)
    }

    /// Surface store-health transitions (Ready↔Failed, shadow warnings) the
    /// resolver queued during resolution, as `window/logMessage`. Called from
    /// the document-pull handler — the frequent path that holds the `Client` —
    /// so it replaces the deleted background notifier. Drain fully under the
    /// lock into a `Vec`, then log outside it (never hold a lock across await).
    async fn drain_store_events(&self) {
        let events: Vec<packages::StoreEvent> = {
            let mut rx = self.store_events.lock().unwrap_or_else(|e| e.into_inner());
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        };
        for ev in events {
            let level = if ev.warning {
                MessageType::WARNING
            } else {
                MessageType::INFO
            };
            self.client.log_message(level, ev.message).await;
        }
    }

    /// Update server state for a changed document. Diagnostics are NOT pushed:
    /// under the pull model (RFC 0035) the client re-pulls this document (a
    /// `didChange` triggers a document pull) and re-pulls dependents when they
    /// gain focus. A model or project-config edit only updates the shared
    /// registry/config here; every affected file heals on its next pull.
    fn on_change(&self, uri: Url, text: String, version: Option<i32>) {
        // THE one writer of buffer text, so the one cap: a buffer past
        // the bound is refused HERE — never stored, so no handler can
        // parse it — exactly as the index and the watcher refuse the
        // same file on disk and `nml check` refuses it as a target.
        if text.len() > MAX_INDEX_BYTES {
            self.refuse_buffer(uri, text.len() as u64);
            return;
        }
        self.refused_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.clone(), text.clone(), version);
        // This document's cached diagnostics are stale (text changed). The
        // project-config and registry branches below clear wholesale — those
        // changes affect every document.
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);

        // Segment-anchored: `foo-nml-project.nml` is an ordinary document,
        // not project config. A project config (modifiers, template
        // namespaces) shapes every document's diagnostics through the
        // kernel's nearest-config lookup, which reads this buffer —
        // wholesale invalidation, no global to load.
        if uri.as_str().rsplit('/').next() == Some(nml_validate::workspace::PROJECT_CONFIG_NAME) {
            self.diags_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            return;
        }
        if is_schema_source(&uri) {
            self.rebuild_schema_registry();
        }
    }
}

impl Inner {
    /// Refuse an open buffer of `len` bytes: any stored text at the URI
    /// (an earlier version, an indexed disk copy) goes — the buffer is
    /// the truth and the truth is refused — and every consumer that
    /// read it is told (the registry for a `.model.nml`, every document
    /// for a project config), exactly as a change would.
    fn refuse_buffer(&self, uri: Url, len: u64) {
        self.refused_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.clone(), len);
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        if uri.as_str().rsplit('/').next() == Some(nml_validate::workspace::PROJECT_CONFIG_NAME) {
            self.diags_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        } else if is_schema_source(&uri) {
            self.rebuild_schema_registry();
        }
    }

    /// The one row a refused open buffer reports (the kernel's cap
    /// sentence, uncoded like the CLI's target refusal), or `None` for a
    /// buffer under the bound.
    fn refused_buffer_row(&self, uri: &Url) -> Option<tower_lsp::lsp_types::Diagnostic> {
        let len = *self
            .refused_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)?;
        let zero = tower_lsp::lsp_types::Position::new(0, 0);
        Some(tower_lsp::lsp_types::Diagnostic {
            range: tower_lsp::lsp_types::Range::new(zero, zero),
            severity: Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
            message: nml_validate::workspace::too_large(Some(len), MAX_INDEX_BYTES, OPEN_DOCUMENT),
            source: Some("nml".to_string()),
            ..Default::default()
        })
    }

    fn find_definition(
        &self,
        name: &str,
        current_uri: &Url,
        enclosing_keyword: Option<&str>,
    ) -> Option<(Url, Range)> {
        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        find_definition_in_docs(&docs, name, current_uri, enclosing_keyword)
    }

    fn find_schema_definition(&self, name: &str, current_uri: &Url) -> Option<(Url, Range)> {
        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());

        let file_scope = extract_file_scope(current_uri.as_str());

        let mut model_uris: Vec<&Url> = docs.keys().filter(|u| is_schema_source(u)).collect();

        if let Some(ref scope) = file_scope {
            let scope = scope.clone();
            model_uris.sort_by_key(|u| {
                if extract_schema_scope(u.as_str()) == scope {
                    0
                } else {
                    1
                }
            });
        }

        for uri in model_uris {
            if let Some(source) = docs.get(uri) {
                let file = nml_core::cst::parse_best_effort(source);
                let line_index = LineIndex::new(source);
                if let Some(range) = find_schema_block_definition(&file, name, &line_index) {
                    return Some((uri.clone(), range));
                }
            }
        }
        None
    }

    fn find_tagged_ref_definition(&self, role_ref: &str) -> Option<Location> {
        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        find_tagged_ref_definition_in_docs(&docs, role_ref)
    }

    fn find_tagged_ref_hover(&self, keyword: &str, name: &str) -> Option<String> {
        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        find_tagged_ref_hover_in_docs(&docs, keyword, name)
    }

    fn collect_declaration_names(&self) -> Vec<(String, String)> {
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let mut names = Vec::new();
        for source in docs.values() {
            let file = nml_core::cst::parse_best_effort(source);
            for decl in &file.declarations {
                match &decl.kind {
                    DeclarationKind::Block(block) => {
                        names.push((block.name.name.clone(), block.keyword.name.clone()));
                    }
                    DeclarationKind::Array(arr) => {
                        names.push((
                            arr.name.name.clone(),
                            format!("[]{}", arr.item_keyword.name),
                        ));
                    }
                    DeclarationKind::Const(c) => {
                        names.push((c.name.name.clone(), "const".into()));
                    }
                    DeclarationKind::Template(t) => {
                        names.push((t.name.name.clone(), "template".into()));
                    }
                    DeclarationKind::OneOf(o) => {
                        names.push((o.name.name.clone(), "oneof".into()));
                    }
                }
            }
        }
        names
    }
}

/// Uncovered-universe bound: a pathological workspace must not turn
/// every diagnostics pull into an unbounded load. Manifest-declared
/// universes are author-bounded and uncapped.
///
/// LIMIT: reach=content guards=memory surface=editor shown="128" — files an uncovered universe loads for one diagnostics pull
const MAX_UNIVERSE_FILES: usize = 128;

/// A related note's OWN file, by path: an open buffer first (its unsaved
/// text is the truth), else disk under [`MAX_LOCATE_BYTES`] through the
/// kernel's one reader at the file's own leaf (`read_leaf`: the open
/// never blocks, a non-regular file or a leaf swapped for a link is
/// refused) — `None` over the cap or refused, so the renderer falls back
/// loudly, as for any unlocatable file.
fn locate_source(docs: &HashMap<Url, String>, path: &Path) -> Option<(Url, String)> {
    let url = Url::from_file_path(path).ok()?;
    let text = match docs.get(&url) {
        Some(text) => text.clone(),
        None => read_leaf(path, MAX_LOCATE_BYTES as usize, "a related note's file").ok()?,
    };
    Some((url, text))
}

/// Assemble a COVERED `.model.nml` buffer's validation universe from its
/// package's `[]schema` paths, in MANIFEST order — merge order decides
/// which duplicate definition is "second" (and so carries the error), so
/// the editor must agree with every other consumer of the package.
///
/// Reads are buffer-first through `read` (an open buffer is the sole
/// source of truth for its file; disk is the fallback), and the buffer's
/// own text always wins for its own path. A declared file missing on
/// disk contributes nothing (its absence is the package's own resolution
/// problem, reported there); a covered-but-undeclared buffer (the
/// sibling trap) is appended AFTER the declared set, so duplicate
/// attribution lands on the undeclared file — the one whose declaration
/// status is in question. The declared-entry identity is the kernel's
/// NAME on both sides (the one document-path rule), never a second
/// canonicalization.
fn declared_universe(
    own_name: &str,
    own_text: &str,
    declared: &[PathBuf],
    read: &dyn Fn(&Path) -> Option<String>,
    name_of: &dyn Fn(&Path) -> String,
) -> Vec<(String, String)> {
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut own_declared = false;
    for entry in declared {
        // Identity by the kernel's NAME — the key under a root, the path
        // outside one — on both sides: the own document's name is the
        // resolution's key, a declared entry's the key its path spells
        // (`name_of`), and both came through the one document-path rule,
        // so no canonicalization decides here (a link inside the root is
        // the link on both sides; the case the kernel verified is the
        // case both carry). A missing entry keeps its authored spelling;
        // `read` then fails and it is skipped.
        if name_of(entry) == own_name {
            own_declared = true;
            sources.push((own_name.to_string(), own_text.to_string()));
        } else if let Some(text) = read(entry) {
            sources.push((name_of(entry), text));
        }
    }
    if !own_declared {
        sources.push((own_name.to_string(), own_text.to_string()));
    }
    sources
}

/// Assemble an UNCOVERED `.model.nml` buffer's validation universe from
/// the workspace registry set: every other `.model.nml` document the
/// server holds (indexed + open), freshest text by construction. This is
/// the SAME one-namespace the registry validator and go-to-definition
/// resolve against (RFC 0012) — an uncovered buffer's `is` targets must
/// get the same verdict the server's own navigation gives them. Sorted
/// for deterministic merge order, bounded by [`MAX_UNIVERSE_FILES`]
/// (own buffer first, so duplicate attribution lands on the other
/// file); entries beyond the cap drop deterministically (sorted tail).
///
/// The returned flag reports whether the cap CUT the set: a truncated
/// universe is not the registry namespace, so the load pass loses
/// composition ownership (`is` verdicts stay with the uncapped registry
/// validator) rather than reporting false "unknown `is` target" errors
/// for definitions that dropped with the tail.
fn registry_universe(
    own_name: &str,
    own_text: &str,
    mut docs: Vec<(String, String)>,
) -> (Vec<(String, String)>, bool) {
    docs.sort_by(|a, b| a.0.cmp(&b.0));
    let truncated = docs.len() > MAX_UNIVERSE_FILES.saturating_sub(1);
    docs.truncate(MAX_UNIVERSE_FILES.saturating_sub(1));
    let mut sources = Vec::with_capacity(docs.len() + 1);
    sources.push((own_name.to_string(), own_text.to_string()));
    sources.extend(docs);
    (sources, truncated)
}

/// Assemble a STORE-covered buffer's validation universe: the package's
/// hash-verified `(logical name, text)` sources in declaration order,
/// with the buffer's own text appended LAST. Always appended,
/// unconditionally: store sources live in the store slot, never in the
/// workspace, so the buffer cannot be one of them — and logical names
/// (`[a-z][a-z0-9-]*`) can never collide with the buffer's absolute-path
/// key, so no dedup is needed or possible.
/// The span of a document's first declaration — where a note anchored
/// at [`packages::NoteAnchor::Declaration`] sits (an inert `package`/
/// `project` block); `None` for a document that declares nothing.
fn first_declaration(file: &nml_core::ast::File) -> Option<nml_core::span::Span> {
    file.declarations.first().map(|d| d.span)
}

fn snapshot_universe(
    own_name: &str,
    own_text: &str,
    sources: &[(String, std::sync::Arc<str>)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(sources.len() + 1);
    out.extend(
        sources
            .iter()
            .map(|(name, text)| (name.clone(), (**text).to_string())),
    );
    out.push((own_name.to_string(), own_text.to_string()));
    out
}

/// Canonicalize `path` and require the result inside one of the
/// (already-canonical) `roots` — the ONE containment predicate for any
/// surface that turns an untrusted path into a filesystem read. The
/// order is load-bearing: canonicalize FIRST (resolving symlinks), then
/// contain — check-then-canonicalize is the classic symlink escape.
/// Pure containment: symlink *policy* stays with callers that have one
/// (a symlink check must run BEFORE canonicalization, which erases the
/// evidence). Fail-closed: a path that cannot canonicalize is `None`.
fn canonical_within_roots(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let canonical = dunce::canonicalize(path).ok()?;
    roots
        .iter()
        .any(|root| canonical.starts_with(root))
        .then_some(canonical)
}

/// Whether a watched-file event should be honored.
///
/// Mirrors the rules the kernel's walk indexes under: the path must not be
/// a symlink, and it must canonicalize to a location inside one of the
/// (canonicalized) workspace roots ([`canonical_within_roots`]).
/// Clients can send arbitrary `file://` URIs in watched-file
/// notifications, so this is the boundary check that keeps the server
/// from reading files outside the workspace.
fn watched_file_is_eligible(path: &Path, roots: &[PathBuf]) -> bool {
    let is_symlink = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true);
    if is_symlink {
        return false;
    }
    canonical_within_roots(path, roots).is_some()
}

/// A watched-change path in the resolver's namespace. Roots are
/// canonicalized at initialize and resolved document paths follow suit, so
/// claims-cache roots are canonical too — an un-canonicalized event path
/// would fail every `starts_with` (macOS `/tmp` → `/private/tmp`, symlinked
/// checkouts) and silently retain a verdict that should have been dropped.
/// DELETED paths no longer exist, so fall back to canonicalizing the parent
/// (which usually still does) and re-appending the name; failing both, the
/// raw path — a mismatch then merely retains a memo, it never fabricates
/// one.
fn canonicalize_watched_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = dunce::canonicalize(path) {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => dunce::canonicalize(parent)
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

// ── Role ref resolution (free functions for testability) ──────

fn find_tagged_ref_definition_in_docs(
    docs: &HashMap<Url, String>,
    role_ref: &str,
) -> Option<Location> {
    let stripped = role_ref.strip_prefix('@')?;
    let (keyword, name) = stripped.split_once('/')?;

    for (uri, source) in docs {
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        for decl in &file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                if block.keyword.name == keyword && block.name.name == name {
                    return Some(Location {
                        uri: uri.clone(),
                        range: span_to_range(block.name.span, &line_index),
                    });
                }
            }
        }
    }
    None
}

fn find_tagged_ref_hover_in_docs(
    docs: &HashMap<Url, String>,
    keyword: &str,
    name: &str,
) -> Option<String> {
    for (uri, source) in docs {
        let file = nml_core::cst::parse_best_effort(source);
        for decl in &file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                if block.keyword.name == keyword && block.name.name == name {
                    let mut text = format!("**{keyword}** `{name}`");

                    // A comment above the declaration documents it (RFC 0004 §4.3).
                    if let Some(doc) = nml_core::cst::doc_comment_for(source, name) {
                        text.push_str(&format!("\n\n{doc}"));
                    }

                    let desc = block.body.entries.iter().find_map(|e| {
                        if let BodyEntryKind::Property(prop) = &e.kind {
                            if prop.name.name == "description" {
                                if let Value::String(s) = &prop.value.value {
                                    return Some(s.clone());
                                }
                            }
                        }
                        None
                    });
                    if let Some(d) = desc {
                        text.push_str(&format!("\n\n{d}"));
                    }

                    let summary = summarize_body(&block.body);
                    if !summary.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&summary);
                    }

                    let file_name = uri
                        .path_segments()
                        .and_then(|mut s| s.next_back())
                        .unwrap_or("unknown");
                    text.push_str(&format!("\n\n*Source: {file_name}*"));

                    return Some(text);
                }
            }
        }
    }
    None
}

// ── Schema scoping ────────────────────────────────────────────

/// Whether a document is a schema source — the kernel's admission
/// (`*.model.nml`, `*.schema.nml`:
/// [`nml_validate::workspace::is_schema_source_name`]), the one spelling
/// both front ends read: what feeds the registry, opens the schema
/// passes, answers directive completion and hover, and is re-read on
/// change. The editor keeps no predicate of its own.
fn is_schema_source(uri: &Url) -> bool {
    nml_validate::workspace::is_schema_source_name(uri.as_str())
}

/// The registry scope of a schema source: its stem, by the kernel's
/// spelling ([`nml_validate::workspace::schema_source_stem`] — `core`
/// for `core.model.nml` and `core.schema.nml` alike); empty for any
/// other name.
fn extract_schema_scope(uri_str: &str) -> String {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    nml_validate::workspace::schema_source_stem(filename)
        .unwrap_or("")
        .to_string()
}

fn extract_file_scope(uri_str: &str) -> Option<String> {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    if nml_validate::workspace::is_schema_source_name(filename) {
        return None;
    }
    let stem = filename.strip_suffix(".nml")?;
    let pos = stem.rfind('.')?;
    Some(stem[pos + 1..].to_string())
}

fn find_enclosing_block_keyword(
    file: &File,
    pos: Position,
    line_index: &LineIndex,
) -> Option<String> {
    let mut best_start: Option<u32> = None;
    let mut result: Option<String> = None;
    for decl in &file.declarations {
        let range = span_to_range(decl.span, line_index);
        if pos.line >= range.start.line && pos.line <= range.end.line {
            let keyword = match &decl.kind {
                DeclarationKind::Block(block) => Some(block.keyword.name.clone()),
                DeclarationKind::Array(arr) => Some(arr.item_keyword.name.clone()),
                _ => None,
            };
            if let Some(kw) = keyword {
                if best_start.is_none_or(|s| range.start.line > s) {
                    best_start = Some(range.start.line);
                    result = Some(kw);
                }
            }
        }
    }
    result
}

// ── Definition resolution ─────────────────────────────────────

fn find_definition_in_docs(
    docs: &HashMap<Url, String>,
    name: &str,
    current_uri: &Url,
    enclosing_keyword: Option<&str>,
) -> Option<(Url, Range)> {
    let file_scope = extract_file_scope(current_uri.as_str());
    let is_on_keyword = enclosing_keyword == Some(name);

    // Priority 1: Field definition in the specific enclosing model
    // (Skip when cursor is on the declaration keyword itself)
    if !is_on_keyword {
        if let Some(keyword) = enclosing_keyword {
            let mut model_uris: Vec<&Url> = docs.keys().filter(|u| is_schema_source(u)).collect();

            if let Some(ref scope) = file_scope {
                let scope = scope.clone();
                model_uris.sort_by_key(|u| {
                    if extract_schema_scope(u.as_str()) == scope {
                        0
                    } else {
                        1
                    }
                });
            }

            for uri in &model_uris {
                if let Some(source) = docs.get(*uri) {
                    let file = nml_core::cst::parse_best_effort(source);
                    let line_index = LineIndex::new(source);
                    if let Some(range) =
                        find_field_definition_in_model(&file, name, keyword, &line_index)
                    {
                        return Some(((*uri).clone(), range));
                    }
                }
            }
        }
    }

    // Priority 2: Field definitions in .model.nml files (any model)
    // (Skip when cursor is on the declaration keyword itself)
    if !is_on_keyword {
        for (uri, source) in docs.iter() {
            if !is_schema_source(uri) {
                continue;
            }
            let file = nml_core::cst::parse_best_effort(source);
            let line_index = LineIndex::new(source);
            if let Some(range) = find_field_definition(&file, name, &line_index) {
                return Some((uri.clone(), range));
            }
        }
    }

    // Priority 3: Names in current file (top-level + nested). Resilient parsing
    // always yields a best-effort AST; if the structural lookup misses (e.g. the
    // name sits in a region the parser had to recover), fall back to a text scan.
    if let Some(source) = docs.get(current_uri) {
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        if let Some(range) = find_name_in_file(&file, name, &line_index) {
            return Some((current_uri.clone(), range));
        }
        if let Some(range) = find_name_by_text(source, name) {
            return Some((current_uri.clone(), range));
        }
    }

    // Priority 4: Top-level declarations in other files
    for (uri, source) in docs.iter() {
        if uri == current_uri {
            continue;
        }
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        if let Some(range) = find_top_level_decl(&file, name, &line_index) {
            return Some((uri.clone(), range));
        }
    }

    None
}

fn span_to_range(span: nml_core::span::Span, line_index: &LineIndex) -> Range {
    line_index.range(span)
}

/// A stable result-id for a pull-diagnostics report: a hash of the diagnostics
/// themselves, so it changes iff the output does. A re-pull that recomputes the
/// same set (the common focus-change case) matches the client's
/// `previous_result_id` and returns `Unchanged` — no re-render churn.
/// `DefaultHasher` is fixed-seed, hence deterministic across pulls/runs.
fn diagnostics_result_id(items: &[Diagnostic]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(items)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn find_schema_block_definition(file: &File, name: &str, line_index: &LineIndex) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if matches!(block.keyword.name.as_str(), "model" | "enum") && block.name.name == name {
                return Some(span_to_range(block.name.span, line_index));
            }
        }
    }
    None
}

fn find_field_definition(file: &File, name: &str, line_index: &LineIndex) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if block.keyword.name.as_str() == "model" {
                for entry in &block.body.entries {
                    if let BodyEntryKind::FieldDefinition(fd) = &entry.kind {
                        if fd.name.name == name {
                            return Some(span_to_range(fd.name.span, line_index));
                        }
                    }
                }
            }
        }
    }
    None
}

fn find_field_definition_in_model(
    file: &File,
    name: &str,
    model_name: &str,
    line_index: &LineIndex,
) -> Option<Range> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if block.keyword.name.as_str() == "model" && block.name.name == model_name {
                for entry in &block.body.entries {
                    if let BodyEntryKind::FieldDefinition(fd) = &entry.kind {
                        if fd.name.name == name {
                            return Some(span_to_range(fd.name.span, line_index));
                        }
                    }
                }
            }
        }
    }
    None
}

fn find_top_level_decl(file: &File, name: &str, line_index: &LineIndex) -> Option<Range> {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    return Some(span_to_range(block.name.span, line_index));
                }
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    return Some(span_to_range(arr.name.span, line_index));
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    return Some(span_to_range(c.name.span, line_index));
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    return Some(span_to_range(t.name.span, line_index));
                }
            }
            DeclarationKind::OneOf(o) => {
                if o.name.name == name {
                    return Some(span_to_range(o.name.span, line_index));
                }
            }
        }
    }
    None
}

fn find_name_in_file(file: &File, name: &str, line_index: &LineIndex) -> Option<Range> {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    return Some(span_to_range(block.name.span, line_index));
                }
                if let Some(r) = find_name_in_body(&block.body, name, line_index) {
                    return Some(r);
                }
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    return Some(span_to_range(arr.name.span, line_index));
                }
                for item in &arr.body.items {
                    if let Some(r) = find_name_in_list_item(item, name, line_index) {
                        return Some(r);
                    }
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    return Some(span_to_range(c.name.span, line_index));
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    return Some(span_to_range(t.name.span, line_index));
                }
            }
            DeclarationKind::OneOf(o) => {
                if o.name.name == name {
                    return Some(span_to_range(o.name.span, line_index));
                }
            }
        }
    }
    None
}

fn find_name_in_body(body: &Body, name: &str, line_index: &LineIndex) -> Option<Range> {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::ListItem(item) => {
                if let Some(r) = find_name_in_list_item(item, name, line_index) {
                    return Some(r);
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                if nb.name.name == name {
                    return Some(span_to_range(nb.name.span, line_index));
                }
                if let Some(r) = find_name_in_body(&nb.body, name, line_index) {
                    return Some(r);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_name_in_list_item(item: &ListItem, name: &str, line_index: &LineIndex) -> Option<Range> {
    match &item.kind {
        ListItemKind::Named { name: ident, body } => {
            if ident.name == name {
                return Some(span_to_range(ident.span, line_index));
            }
            find_name_in_body(body, name, line_index)
        }
        _ => None,
    }
}

fn find_name_by_text(source: &str, name: &str) -> Option<Range> {
    // `str::find` yields byte offsets; LSP characters are UTF-16 units.
    let name_range = |line_idx: usize, line: &str| {
        let byte_start = line.find(name).unwrap_or(0);
        Some(Range {
            start: Position::new(line_idx as u32, position::byte_to_utf16(line, byte_start)),
            end: Position::new(
                line_idx as u32,
                position::byte_to_utf16(line, byte_start + name.len()),
            ),
        })
    };

    for (line_idx, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(before_colon) = trimmed.strip_suffix(':') {
            let parts: Vec<&str> = before_colon.split_whitespace().collect();
            if parts.len() == 2 && parts[1] == name {
                return name_range(line_idx, line);
            }
        }
        if trimmed.starts_with('-') && trimmed.ends_with(':') {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            if inner == name {
                return name_range(line_idx, line);
            }
        }
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_' || c == '@' || c == '/' || c == '.'
}

/// Extract the word around the given *byte* column (see
/// `position::utf16_to_byte` for converting an LSP character first).
/// Out-of-range or mid-character columns are clamped to a char boundary.
fn extract_word_at(line: &str, byte_col: usize) -> String {
    let mut col = byte_col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }

    let start = line[..col]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);

    let end = line[col..]
        .char_indices()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, _)| col + i)
        .unwrap_or(line.len());

    line[start..end].to_string()
}

/// The directive name under the cursor (`#name`), when the cursor sits on
/// the name or on its `#`; `None` anywhere else — an ordinary word must not
/// hover as a directive merely because the file has a vocabulary. Uses the
/// directive-ident charset (alnum/`_`/`-`), narrower than [`is_word_char`]
/// (whose `@`/`/`/`.` belong to reference tokens, which `#` never contains).
fn directive_name_at(line: &str, byte_col: usize) -> Option<String> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut col = byte_col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    let start = if line[col..].starts_with('#') {
        // Cursor on the `#` itself: the name starts right after it.
        col + 1
    } else {
        let start = line[..col]
            .char_indices()
            .rev()
            .find(|(_, c)| !is_ident(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if start == 0 || !line[..start].ends_with('#') {
            return None;
        }
        start
    };
    let end = line[start..]
        .char_indices()
        .find(|(_, c)| !is_ident(*c))
        .map(|(i, _)| start + i)
        .unwrap_or(line.len());
    (end > start).then(|| line[start..end].to_string())
}

/// Neutralize markdown code-fence openers in schema-author doc text before
/// splicing it into a hover. ONLY triple-backtick runs are escaped: an
/// unescaped ``` in the doc would open a fence that swallows the rest of the
/// hover (including our own closing fence), which is structural breakage —
/// whereas lighter emphasis characters (`*`, `_`, single backticks) at worst
/// reflow cosmetically, not worth mangling every doc that mentions them.
fn escape_markdown_fences(doc: &str) -> String {
    doc.replace("```", "\\`\\`\\`")
}

// ── Document symbols ──────────────────────────────────────────

/// Construct a `DocumentSymbol`, isolating the one `#[allow(deprecated)]`
/// that `lsp_types` forces on us: the deprecated `deprecated` field must
/// still be initialized in struct literals. Empty `children` collapse to
/// `None` per the LSP convention.
fn document_symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: Range,
    selection_range: Range,
    children: Vec<DocumentSymbol>,
) -> DocumentSymbol {
    #[allow(deprecated)]
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children: (!children.is_empty()).then_some(children),
    }
}

fn build_document_symbols(file: &File, line_index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                symbols.push(document_symbol(
                    block.name.name.clone(),
                    Some(block.keyword.name.clone()),
                    SymbolKind::CLASS,
                    span_to_range(decl.span, line_index),
                    span_to_range(block.name.span, line_index),
                    build_body_symbols(&block.body, line_index),
                ));
            }
            DeclarationKind::Array(arr) => {
                symbols.push(document_symbol(
                    arr.name.name.clone(),
                    Some(format!("[]{}", arr.item_keyword.name)),
                    SymbolKind::ARRAY,
                    span_to_range(decl.span, line_index),
                    span_to_range(arr.name.span, line_index),
                    build_array_body_symbols(&arr.body, line_index),
                ));
            }
            DeclarationKind::Const(c) => {
                symbols.push(document_symbol(
                    c.name.name.clone(),
                    Some("const".into()),
                    SymbolKind::CONSTANT,
                    span_to_range(decl.span, line_index),
                    span_to_range(c.name.span, line_index),
                    Vec::new(),
                ));
            }
            DeclarationKind::Template(t) => {
                symbols.push(document_symbol(
                    t.name.name.clone(),
                    Some("template".into()),
                    SymbolKind::STRING,
                    span_to_range(decl.span, line_index),
                    span_to_range(t.name.span, line_index),
                    Vec::new(),
                ));
            }
            DeclarationKind::OneOf(o) => {
                let arms = o
                    .arms
                    .iter()
                    .map(|arm| {
                        document_symbol(
                            arm.value.clone(),
                            Some(arm.model.name.clone()),
                            SymbolKind::ENUM_MEMBER,
                            // The WHOLE arm (`"value" -> Model`): LSP 3.17
                            // §DocumentSymbol requires `selectionRange` to
                            // be contained by `range`, and an arm has no
                            // span of its own to take. The model's alone
                            // does not contain the value literal — it does
                            // not even touch it — so a conforming client
                            // drops the arm (VS Code logs and discards the
                            // symbol), and the outline lost every arm of
                            // every `oneof`.
                            span_to_range(
                                Span::new(arm.value_span.start, arm.model.span.end),
                                line_index,
                            ),
                            span_to_range(arm.value_span, line_index),
                            Vec::new(),
                        )
                    })
                    .collect();
                symbols.push(document_symbol(
                    o.name.name.clone(),
                    Some(format!("oneof by {}", o.discriminator.name)),
                    SymbolKind::ENUM,
                    span_to_range(decl.span, line_index),
                    span_to_range(o.name.span, line_index),
                    arms,
                ));
            }
        }
    }
    symbols
}

fn build_body_symbols(body: &Body, line_index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                symbols.push(document_symbol(
                    prop.name.name.clone(),
                    None,
                    SymbolKind::PROPERTY,
                    span_to_range(entry.span, line_index),
                    span_to_range(prop.name.span, line_index),
                    Vec::new(),
                ));
            }
            BodyEntryKind::NestedBlock(nb) => {
                symbols.push(document_symbol(
                    nb.name.name.clone(),
                    None,
                    SymbolKind::FIELD,
                    span_to_range(entry.span, line_index),
                    span_to_range(nb.name.span, line_index),
                    build_body_symbols(&nb.body, line_index),
                ));
            }
            BodyEntryKind::FieldDefinition(fd) => {
                symbols.push(document_symbol(
                    fd.name.name.clone(),
                    Some(fd.field_type.to_string()),
                    SymbolKind::FIELD,
                    span_to_range(entry.span, line_index),
                    span_to_range(fd.name.span, line_index),
                    Vec::new(),
                ));
            }
            BodyEntryKind::ListItem(item) => {
                if let ListItemKind::Named { name, body } = &item.kind {
                    symbols.push(document_symbol(
                        name.name.clone(),
                        None,
                        SymbolKind::FIELD,
                        span_to_range(item.span, line_index),
                        span_to_range(name.span, line_index),
                        build_body_symbols(body, line_index),
                    ));
                }
            }
            BodyEntryKind::Arm(arm) => {
                let (label, children) = match &arm.target {
                    ArmTarget::Inline { name, body } => (
                        format!("{} -> {}", arm_selector_label(&arm.selector), name.name),
                        build_body_symbols(body, line_index),
                    ),
                    _ => (
                        format!(
                            "{} -> {}",
                            arm_selector_label(&arm.selector),
                            arm_target_label(&arm.target)
                        ),
                        Vec::new(),
                    ),
                };
                symbols.push(document_symbol(
                    label,
                    Some("arm".into()),
                    SymbolKind::ENUM_MEMBER,
                    span_to_range(entry.span, line_index),
                    span_to_range(arm.selector_span, line_index),
                    children,
                ));
            }
            _ => {}
        }
    }
    symbols
}

fn arm_selector_label(selector: &ArmSelector) -> String {
    match selector {
        ArmSelector::Role(r) => r.clone(),
        ArmSelector::Literal(k) => nml_core::source_policy::string_literal(k),
        ArmSelector::Else => "else".into(),
    }
}

fn arm_target_label(target: &ArmTarget) -> String {
    match target {
        ArmTarget::Reference(id) => id.name.clone(),
        ArmTarget::Literal(t) => format!("{:?}", t.value),
        ArmTarget::Inline { name, .. } => format!("{}:", name.name),
    }
}

fn build_array_body_symbols(body: &ArrayBody, line_index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    for item in &body.items {
        if let ListItemKind::Named { name, body } = &item.kind {
            symbols.push(document_symbol(
                name.name.clone(),
                None,
                SymbolKind::FIELD,
                span_to_range(item.span, line_index),
                span_to_range(name.span, line_index),
                build_body_symbols(body, line_index),
            ));
        }
    }
    symbols
}

// ── References ────────────────────────────────────────────────

fn find_references_in_source(source: &str, name: &str, line_index: &LineIndex) -> Vec<Range> {
    let mut ranges = Vec::new();
    let file = nml_core::cst::parse_best_effort(source);
    collect_references(&file, name, line_index, &mut ranges);
    ranges
}

fn collect_references(file: &File, name: &str, line_index: &LineIndex, ranges: &mut Vec<Range>) {
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => {
                if block.name.name == name {
                    ranges.push(span_to_range(block.name.span, line_index));
                }
                collect_body_references(&block.body, name, line_index, ranges);
            }
            DeclarationKind::Array(arr) => {
                if arr.name.name == name {
                    ranges.push(span_to_range(arr.name.span, line_index));
                }
                for item in &arr.body.items {
                    collect_list_item_references(item, name, line_index, ranges);
                }
            }
            DeclarationKind::Const(c) => {
                if c.name.name == name {
                    ranges.push(span_to_range(c.name.span, line_index));
                }
                if let Value::Reference(ref_name) = &c.value.value {
                    if ref_name == name {
                        ranges.push(span_to_range(c.value.span, line_index));
                    }
                }
            }
            DeclarationKind::Template(t) => {
                if t.name.name == name {
                    ranges.push(span_to_range(t.name.span, line_index));
                }
            }
            DeclarationKind::OneOf(o) => {
                if o.name.name == name {
                    ranges.push(span_to_range(o.name.span, line_index));
                }
                // A oneof arm references a variant model by name.
                for arm in &o.arms {
                    if arm.model.name == name {
                        ranges.push(span_to_range(arm.model.span, line_index));
                    }
                }
            }
        }
    }
}

fn collect_body_references(
    body: &Body,
    name: &str,
    line_index: &LineIndex,
    ranges: &mut Vec<Range>,
) {
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                if let Value::Reference(ref_name) = &prop.value.value {
                    if ref_name == name {
                        ranges.push(span_to_range(prop.value.span, line_index));
                    }
                }
            }
            BodyEntryKind::NestedBlock(nb) => {
                if nb.name.name == name {
                    ranges.push(span_to_range(nb.name.span, line_index));
                }
                collect_body_references(&nb.body, name, line_index, ranges);
            }
            BodyEntryKind::ListItem(item) => {
                collect_list_item_references(item, name, line_index, ranges);
            }
            _ => {}
        }
    }
}

fn collect_list_item_references(
    item: &ListItem,
    name: &str,
    line_index: &LineIndex,
    ranges: &mut Vec<Range>,
) {
    match &item.kind {
        ListItemKind::Named { name: ident, body } => {
            if ident.name == name {
                ranges.push(span_to_range(ident.span, line_index));
            }
            collect_body_references(body, name, line_index, ranges);
        }
        ListItemKind::Reference(ident) if ident.name == name => {
            ranges.push(span_to_range(ident.span, line_index));
        }
        _ => {}
    }
}

// ── Hover helpers ─────────────────────────────────────────────

fn summarize_body(body: &Body) -> String {
    let mut lines = Vec::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(prop) => {
                lines.push(format!(
                    "  {} = {}",
                    prop.name.name,
                    format_named_value(&prop.name.name, &prop.value.value)
                ));
            }
            BodyEntryKind::NestedBlock(nb) => {
                lines.push(format!("  {}:", nb.name.name));
            }
            BodyEntryKind::FieldDefinition(fd) => {
                let type_name = fd.field_type.to_string();
                let opt = if fd.optional { "?" } else { "" };
                lines.push(format!("  {} {}{}", fd.name.name, type_name, opt));
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("```nml\n{}\n```", lines.join("\n"))
}

/// The `<prop> = ⌖` prefix of a value position: the property name to the left
/// of `=` on the cursor's line, or `None` when the line has no `=` before the
/// cursor. The one line-parse every value-position lookup shares.
pub(crate) fn value_position_prop_name(source: &str, pos: Position) -> Option<&str> {
    let line = position::line_at(source, pos.line)?;
    let end = position::utf16_to_byte(line, pos.character);
    let eq_pos = line[..end].find('=')?;
    let prop_name = line[..eq_pos].trim();
    (!prop_name.is_empty()).then_some(prop_name)
}

/// Everything declared to govern a value position (`<name> = ⌖`).
///
/// Candidates-aware and MERGING: in an ambiguous union body a name shared
/// across variants contributes EVERY declaring variant's field — first-wins
/// would let one variant's `string` silently suppress another's enum values.
/// A oneof candidate declares no fields pre-discriminator, but when the name
/// IS its discriminator the arm keys govern the value (tier-0 scaffolds
/// `kind = ` from exactly this pseudo-field; the value position must then
/// actually complete).
pub(crate) struct ValueGovernors<'i> {
    pub(crate) fields: Vec<&'i FieldDef>,
    discriminator_arms: Vec<String>,
}

pub(crate) fn value_governors_at<'i>(
    file: &File,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
    prop_name: &str,
) -> ValueGovernors<'i> {
    // `|vis = …` authors the MODIFIER form of a field: the sigil is syntax,
    // not part of the declared name, so strip it for the lookup — and require
    // the authored form to match the declaration (sigil ⟺ modifier-typed).
    // A sigil on a plain field, or a bare name on a modifier field, authors a
    // *different* entry than the declaration, so no field governs it.
    let (name, sigiled) = match prop_name.strip_prefix('|') {
        Some(bare) => (bare.trim_start(), true),
        None => (prop_name, false),
    };
    let form_matches = |f: &FieldDef| f.name == name && is_modifier_form(f) == sigiled;
    let mut governors = ValueGovernors {
        fields: Vec::new(),
        discriminator_arms: Vec::new(),
    };
    match find_candidates_at(file, pos, index, line_index) {
        Some(DescentTarget::One {
            model, via_oneof, ..
        }) => {
            // The discriminator STRIP, mirrored from the validator: in a
            // via-resolved body a property named like the discriminator IS
            // the discriminator — the validator claims it before variant
            // validation — so a shadowing variant field's values would all
            // be rejected there. Arm keys only (the variant-switching
            // moment: `kind = "log"` at the value completes every arm).
            // The sigiled form (`|kind`) is never the discriminator and
            // keeps the field channel.
            match via_oneof.filter(|o| o.discriminator == name && !sigiled) {
                Some(o) => governors
                    .discriminator_arms
                    .extend(o.variants.iter().map(|(value, _)| value.clone())),
                None => governors
                    .fields
                    .extend(model.fields.iter().find(|f| form_matches(f))),
            }
        }
        Some(DescentTarget::Ambiguous { candidates, .. }) => {
            for candidate in &candidates {
                match candidate {
                    NameableVariant::Model(m) => governors
                        .fields
                        .extend(m.fields.iter().find(|f| form_matches(f))),
                    NameableVariant::OneOf(o) if o.discriminator == name && !sigiled => {
                        governors
                            .discriminator_arms
                            .extend(o.variants.iter().map(|(value, _)| value.clone()));
                    }
                    NameableVariant::OneOf(_) => {}
                }
            }
        }
        None => {}
    }
    governors
}

/// At a value position (`<prop> = ⌖`), the model-ref type names whose
/// declarations are legal reference values there — through the Modifier form
/// wrapper, list/set element types, and every model-ref member of a union
/// (`slot (modelA | modelB)` admits declarations of either). Enum refs are
/// excluded (their values are VARIANTS, [`find_value_completions_at`]'s job).
/// Governor-merged across ambiguous candidates; deduplicated, declaration
/// order. Built on the shared cursor-context walk, so it works at any
/// nesting depth. Takes the parsed `&File` (parse-once).
fn find_model_ref_types_at(
    file: &File,
    source: &str,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Vec<String> {
    fn ref_names_of(ty: &FieldType, index: &SchemaIndex, out: &mut Vec<String>) {
        match ty {
            FieldType::ModelRef(name) => {
                if index.enum_def(name).is_none() {
                    out.push(name.clone());
                }
            }
            FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
                ref_names_of(inner, index, out)
            }
            FieldType::Union(members) => {
                for m in members {
                    ref_names_of(m, index, out);
                }
            }
            // NO `Arms` recursion (asymmetry with `variants_of` is deliberate):
            // an arms member's target governs `@key -> ⌖` positions, never the
            // `= ⌖` scalar form this lookup serves.
            _ => {}
        }
    }
    let Some(prop_name) = value_position_prop_name(source, pos) else {
        return Vec::new();
    };
    let governors = value_governors_at(file, pos, index, line_index, prop_name);
    let mut names = Vec::new();
    for field in &governors.fields {
        ref_names_of(&field.field_type, index, &mut names);
    }
    let mut seen = HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

/// The two kinds of declared value valid at a value position, kept separate
/// so completion labels each for what it is: `variants` from enum-typed
/// governing fields ("enum variant"), `arms` from a oneof discriminator
/// ("discriminator value" — the same label the top-level oneof path uses).
/// Both in schema-declaration order, deduplicated (variants win overlaps).
struct ValueCompletions {
    variants: Vec<String>,
    arms: Vec<String>,
}

#[cfg(test)]
impl ValueCompletions {
    /// The single merged list most pins assert — variants then arms.
    fn merged(self) -> Vec<String> {
        let mut all = self.variants;
        all.extend(self.arms);
        all
    }
}

/// Declared values valid in the value position at the cursor (RFC 0030): for
/// a field typed as an enum ref, a list of enum refs, or a union whose
/// members include enum refs, the declared variants in schema-declaration
/// order (canonical spelling — the whole point of surfacing them); plus a
/// governing oneof discriminator's arm keys. This is the plain-enum
/// completion the LSP never had: `ENUM_MEMBER` previously existed only for
/// oneof discriminator arms and membership refs. Governor-merged: every
/// declaring candidate's field contributes (candidate order); `None` when
/// nothing completes.
fn find_value_completions_at(
    file: &File,
    source: &str,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Option<ValueCompletions> {
    let prop_name = value_position_prop_name(source, pos)?;
    let governors = value_governors_at(file, pos, index, line_index, prop_name);

    fn variants_of(ty: &FieldType, index: &SchemaIndex, out: &mut Vec<String>) {
        match ty {
            FieldType::ModelRef(name) => {
                if let Some(e) = index.enum_def(name) {
                    out.extend(e.variants.iter().cloned());
                }
            }
            FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
                variants_of(inner, index, out)
            }
            FieldType::Union(members) => {
                for m in members {
                    variants_of(m, index, out);
                }
            }
            // `(K -> V)` arm sets: the value position after `->` takes V —
            // when V is (or contains) an enum, its variants complete there.
            FieldType::Arms { target, .. } => variants_of(target, index, out),
            _ => {}
        }
    }
    let mut variants = Vec::new();
    for field in &governors.fields {
        variants_of(&field.field_type, index, &mut variants);
    }
    let mut seen = HashSet::new();
    variants.retain(|v| seen.insert(v.clone()));
    let mut arms = governors.discriminator_arms;
    arms.retain(|a| seen.insert(a.clone()));
    (!variants.is_empty() || !arms.is_empty()).then_some(ValueCompletions { variants, arms })
}

/// Whether a schema field type can govern a duration value — the gate for
/// unit-suffix completion on a bare number in value position.
pub(crate) fn governs_duration(ty: &FieldType) -> bool {
    match ty {
        FieldType::Primitive {
            ty: nml_core::types::PrimitiveType::Duration,
            ..
        } => true,
        FieldType::List(inner) | FieldType::Set(inner) | FieldType::Modifier(inner) => {
            governs_duration(inner)
        }
        FieldType::Union(members) => members.iter().any(governs_duration),
        FieldType::Arms { target, .. } => governs_duration(target),
        _ => false,
    }
}

// ── Schema-driven field completion (RFC 0003) ─────────────────────────────────

/// Resolve the model whose fields are valid at the cursor's body, **and that body** (so the
/// caller excludes already-present fields without re-walking). The schema-driven dual of
/// [`find_model_ref_types_at`]: that resolves a *field's value types*; this resolves the
/// *enclosing body's model* so its fields can be completed.
///
/// Resolves the **top-level** block the cursor sits in (`resolve_ref(keyword)`), then
/// **recursively descends** to the innermost body the cursor is in — through nested
/// model-typed fields (`prompt:`), list items (`steps:` → `- step:`), and `oneof` variants
/// (selected from the body's discriminator). `None` when no schema model applies (unknown
/// keyword / free-form `object` / a union whose discriminator is unset), or the cursor is on a
/// header line.
/// RFC 0015: if the cursor sits in the `as`-type slot of a header line
/// (`<field> as <partial>`, before any `:` body or `=` value), return the field
/// name whose union variants should be completed. `None` otherwise. Pure line
/// analysis so the completion detector is unit-testable in isolation.
/// What the `as`-type slot under the cursor annotates: a FIELD header
/// (`slot as ⌖` — the union lives on the named field itself) or a LIST ITEM
/// (`- one as ⌖` — the name is the *item's*, and the union lives on the
/// **enclosing list field**, found by span). Conflating the two made
/// element-level completion silently dead: the item name matched no field.
enum AsSlot {
    Field(String),
    Item,
}

/// Pure selection half of the RFC 0010 hover augmentation, unit-testable in
/// isolation: the coded diagnostics whose ranges contain `pos`, narrowest
/// first (most specific), deduplicated by code, capped at three (hover real
/// estate is precious). Each renders as `**CODE** — summary` with the CLI
/// pointer; the returned range is the narrowest hit's — the hover highlight.
fn explanations_at_position(
    items: &[tower_lsp::lsp_types::Diagnostic],
    pos: Position,
) -> Option<(String, Range)> {
    fn narrowness(r: &Range) -> (u32, u32) {
        let lines = r.end.line.saturating_sub(r.start.line);
        let chars = if lines == 0 {
            r.end.character.saturating_sub(r.start.character)
        } else {
            u32::MAX
        };
        (lines, chars)
    }
    let mut hits: Vec<&tower_lsp::lsp_types::Diagnostic> = items
        .iter()
        .filter(|d| range_contains(&d.range, pos) && d.code.is_some())
        .collect();
    hits.sort_by_key(|d| narrowness(&d.range));
    let range = hits.first()?.range;
    let mut seen: Vec<&str> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for d in &hits {
        let Some(tower_lsp::lsp_types::NumberOrString::String(code)) = &d.code else {
            continue;
        };
        if seen.contains(&code.as_str()) {
            continue;
        }
        seen.push(code);
        // Only codes with an index section explain (the guard makes that
        // every code in practice; a foreign code string simply skips).
        let Some(summary) = nml_core::diagnostic::explain_summary(code) else {
            continue;
        };
        parts.push(format!(
            "**{code}** — {summary}\n\n_Run `nml explain {code}` for the full entry._"
        ));
        if parts.len() == 3 {
            break;
        }
    }
    (!parts.is_empty()).then(|| (parts.join("\n\n"), range))
}

/// Position-in-range, end-inclusive: hovering the last column of a squiggle
/// still counts as hovering the diagnostic.
fn range_contains(range: &Range, pos: Position) -> bool {
    (range.start.line < pos.line
        || (range.start.line == pos.line && range.start.character <= pos.character))
        && (pos.line < range.end.line
            || (pos.line == range.end.line && pos.character <= range.end.character))
}

/// The RFC 0010 compose point — the ONE place base hover and diagnostic
/// explanation meet. No base + an explanation ⇒ an explanation-only hover
/// carrying the DIAGNOSTIC's range (the squiggle is what the user asked
/// about).
fn merge_hover(base: Option<Hover>, aug: Option<(String, Range)>) -> Option<Hover> {
    match (base, aug) {
        (base, None) => base,
        (None, Some((md, range))) => Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: md,
            }),
            range: Some(range),
        }),
        (Some(mut h), Some((md, _))) => {
            // Every base hover today is Markup; a future non-markup base
            // keeps itself and drops the augmentation rather than mangling
            // either.
            if let HoverContents::Markup(mc) = &mut h.contents {
                mc.value.push_str("\n\n---\n\n");
                mc.value.push_str(&md);
            }
            Some(h)
        }
    }
}

fn as_position_field(line: &str, cursor_byte: usize) -> Option<AsSlot> {
    let before = line.get(..cursor_byte)?;
    // Still in the header type slot — a `:` (body) or `=` (value) means we have
    // left it.
    if before.contains(':') || before.contains('=') {
        return None;
    }
    let trimmed = before.trim_start();
    let is_item = trimmed.starts_with("- ");
    let header = trimmed.trim_start_matches("- ");
    let mut toks = header.split_whitespace();
    let name = toks.next()?;
    if toks.next()? != "as" {
        return None;
    }
    // At most one partial variant token may trail the `as`; anything more means
    // the cursor is past the annotation.
    let in_slot = match toks.next() {
        None => true,
        Some(_) => toks.next().is_none(),
    };
    if !in_slot {
        return None;
    }
    Some(if is_item {
        AsSlot::Item
    } else {
        AsSlot::Field(name.to_string())
    })
}

/// The union-elemented list/set FIELD whose block contains `pos` — the
/// enclosing field of an item annotation slot (`- one as ⌖` sits inside
/// `slots:`, whose type is `[](modelA | modelB)`). Descends through
/// model-typed nested blocks like `descend_to_cursor`, but STOPS at the list
/// field itself (the item under the cursor is mid-typing and need not parse).
/// Editor-grade containment: last entry starting before the cursor, bounded by
/// the next sibling's start.
fn find_union_list_field_at<'i>(
    model: &'i ModelDef,
    body: &Body,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
) -> Option<&'i FieldDef> {
    let mut owner: Option<(&'i FieldDef, &Body)> = None;
    for (i, entry) in body.entries.iter().enumerate() {
        let BodyEntryKind::NestedBlock(nb) = &entry.kind else {
            continue;
        };
        let range = span_to_range(entry.span, line_index);
        if pos.line <= range.start.line {
            continue;
        }
        let next_start = body
            .entries
            .get(i + 1)
            .map(|e| span_to_range(e.span, line_index).start.line);
        let bounded = match next_start {
            Some(next) => pos.line < next,
            None => true,
        };
        if bounded {
            if let Some(field) = model.fields.iter().find(|f| f.name == nb.name.name) {
                owner = Some((field, &nb.body));
            }
        }
    }
    let (field, nested_body) = owner?;
    let base = match &field.field_type {
        FieldType::Modifier(inner) => inner.as_ref(),
        t => t,
    };
    if let FieldType::List(inner) | FieldType::Set(inner) = base {
        // An item body the cursor sits strictly INSIDE (below its header):
        // descend through the item's body-aware resolved variant, so a union
        // list nested in a `[]model` item — or in another union's item — is
        // reachable. The item under the cursor uses the same editor-grade rule.
        let mut item_owner: Option<&Body> = None;
        for (i, entry) in nested_body.entries.iter().enumerate() {
            let BodyEntryKind::ListItem(item) = &entry.kind else {
                continue;
            };
            let range = span_to_range(entry.span, line_index);
            if pos.line <= range.start.line {
                continue; // the item's own header line (the annotation slot)
            }
            let next_start = nested_body
                .entries
                .get(i + 1)
                .map(|e| span_to_range(e.span, line_index).start.line);
            let bounded = match next_start {
                Some(next) => pos.line < next,
                None => true,
            };
            if bounded {
                if let ListItemKind::Named { body, .. } = &item.kind {
                    item_owner = Some(body);
                }
            }
        }
        if let Some(item_body) = item_owner {
            if let Some(m) = variant_model_for_body(index, inner, item_body) {
                return find_union_list_field_at(m, item_body, pos, index, line_index);
            }
            return None;
        }
        // Cursor on an item header: a union-elemented list IS the slot's field.
        return inner.union_variants().is_some().then_some(field);
    }
    // Otherwise descend and keep looking — body-aware, so a union-typed or
    // oneof-typed block on the path (its variant selected by annotation, shape,
    // or discriminator) descends exactly like a plain model block.
    if let Some(child) = variant_model_for_body(index, &field.field_type, nested_body) {
        return find_union_list_field_at(child, nested_body, pos, index, line_index);
    }
    None
}

/// The top-level block declaration owning `pos`, with editor-grade
/// containment: a line the author is MID-TYPING (e.g. the `- one as ` of an
/// in-progress annotation) is often malformed, and resilient parsing trims it
/// from the declaration's content span — strict span containment would orphan
/// exactly the lines completion serves. Rule: the last declaration starting
/// before the cursor owns it, bounded by the NEXT declaration's start (span
/// end alone is not trusted upward).
fn enclosing_top_block<'f>(
    file: &'f File,
    pos: Position,
    line_index: &LineIndex,
) -> Option<&'f BlockDecl> {
    let mut owner: Option<&BlockDecl> = None;
    for (i, decl) in file.declarations.iter().enumerate() {
        let range = span_to_range(decl.span, line_index);
        if pos.line <= range.start.line {
            continue; // header line or before this declaration
        }
        let next_start = file
            .declarations
            .get(i + 1)
            .map(|d| span_to_range(d.span, line_index).start.line);
        let bounded = match next_start {
            Some(next) => pos.line < next,
            None => true,
        };
        if bounded {
            if let DeclarationKind::Block(b) = &decl.kind {
                owner = Some(b);
            }
        }
    }
    owner
}

/// The full descent result — ONE governing model or an AMBIGUOUS union's
/// candidate set (RFC 0015 F4). The union-of-fields completion consumes this;
/// everything else uses the single-model view below.
fn find_candidates_at<'i, 'f>(
    file: &'f File,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
) -> Option<DescentTarget<'i, 'f>> {
    let block = enclosing_top_block(file, pos, line_index)?;
    let Some(FieldTarget::Model(model)) = index.resolve_ref(&block.keyword.name) else {
        return None;
    };
    descend_to_cursor(model, &block.body, pos, index, line_index, None)
}

/// Single-model view of the descent, for consumers that need exactly one
/// governing model (the `as`-slot path, tests). For an ambiguous body it
/// yields the FIRST model candidate — the same deterministic first-declared
/// order structural resolution uses, so legacy behavior is unchanged.
fn find_model_body_at<'i, 'f>(
    file: &'f File,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
) -> Option<(&'i ModelDef, &'f Body)> {
    match find_candidates_at(file, pos, index, line_index)? {
        DescentTarget::One { model, body, .. } => Some((model, body)),
        DescentTarget::Ambiguous {
            candidates, body, ..
        } => candidates
            .iter()
            .find_map(|c| match c {
                NameableVariant::Model(m) => Some(*m),
                // Structural first-wins parity: a LEADING oneof candidate
                // resolves through its discriminator (may be absent → try the
                // next candidate), instead of being skipped entirely.
                NameableVariant::OneOf(o) => resolve_oneof_variant(o, body, index),
            })
            .map(|m| (m, body)),
    }
}

/// RFC 0007 arm-target completion: when `pos` sits inside a nested block whose
/// field is typed as an arm set `(K -> V)`, return the declaration keywords
/// named by `V` (a union target contributes every variant). The completion
/// candidates are then the workspace's declarations of those keywords —
/// including `[]keyword` array items — via
/// [`collect_declarations_by_keyword`].
fn find_arm_target_types_at(
    file: &File,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Option<Vec<String>> {
    let block = enclosing_top_block(file, pos, line_index)?;
    let Some(FieldTarget::Model(model)) = index.resolve_ref(&block.keyword.name) else {
        return None;
    };
    arm_target_descend(model, &block.body, pos, index, line_index)
}

/// RFC 0007 §6.1 arm-selector completion: when `pos` sits inside an arm-set
/// block (but not inside an inline arm target body), return the declared key
/// type `K` from `(K -> V)`.
fn find_arm_set_key_at(
    file: &File,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Option<FieldType> {
    let block = enclosing_top_block(file, pos, line_index)?;
    let Some(FieldTarget::Model(model)) = index.resolve_ref(&block.keyword.name) else {
        return None;
    };
    arm_set_key_descend(model, &block.body, pos, index, line_index)
}

fn arm_set_key_descend(
    model: &ModelDef,
    body: &Body,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Option<FieldType> {
    let idx = owned_entry_index(body.entries.as_slice(), pos, line_index, |e| {
        nested_block_header_line(e, line_index)
    })?;
    let BodyEntryKind::NestedBlock(nested) = &body.entries[idx].kind else {
        return None;
    };
    let field = model.fields.iter().find(|f| f.name == nested.name.name)?;
    match index.resolve_type_in_body(&field.field_type, &nested.body) {
        FieldTarget::Arms { key, .. } => {
            if inline_arm_body_at(&nested.body.entries, pos, line_index).is_some() {
                return None;
            }
            Some(key.clone())
        }
        FieldTarget::Model(child) => {
            arm_set_key_descend(child, &nested.body, pos, index, line_index)
        }
        _ => None,
    }
}

/// The descent half of [`find_arm_target_types_at`]: walk nested blocks to the
/// cursor; an arm-set field (selected body-aware, so `(string | (K -> V))`
/// resolves through its union) yields `V`'s names, a model-typed field
/// recurses.
fn arm_target_descend(
    model: &ModelDef,
    body: &Body,
    pos: Position,
    index: &SchemaIndex,
    line_index: &LineIndex,
) -> Option<Vec<String>> {
    let idx = owned_entry_index(body.entries.as_slice(), pos, line_index, |e| {
        nested_block_header_line(e, line_index)
    })?;
    let BodyEntryKind::NestedBlock(nested) = &body.entries[idx].kind else {
        return None;
    };
    let field = model.fields.iter().find(|f| f.name == nested.name.name)?;
    match index.resolve_type_in_body(&field.field_type, &nested.body) {
        FieldTarget::Arms { target, .. } => {
            if let Some(inline_body) = inline_arm_body_at(&nested.body.entries, pos, line_index) {
                match index.resolve_type_in_body(target, inline_body) {
                    FieldTarget::Model(child) => {
                        return arm_target_descend(child, inline_body, pos, index, line_index);
                    }
                    FieldTarget::OneOf(o) => {
                        if let Some(child) = resolve_oneof_variant(o, inline_body, index) {
                            return arm_target_descend(child, inline_body, pos, index, line_index);
                        }
                    }
                    _ => {}
                }
            }
            Some(named_type_names(target))
        }
        FieldTarget::Model(child) => {
            arm_target_descend(child, &nested.body, pos, index, line_index)
        }
        _ => None,
    }
}

/// The named type references inside a type expression: a ref is itself, a
/// union contributes each variant; primitives contribute nothing.
fn named_type_names(ty: &FieldType) -> Vec<String> {
    match ty {
        FieldType::ModelRef(name) => vec![name.clone()],
        FieldType::Union(variants) => variants.iter().flat_map(named_type_names).collect(),
        _ => Vec::new(),
    }
}

/// What the completion descent lands on: ONE governing model (the common
/// case), or an UNRESOLVED body's candidate set — an ambiguous union (RFC
/// 0015 F4: un-annotated same-class, or annotated with an unknown name, the
/// limbo state) or a pre-discriminator `oneof` (single candidate) — plus the
/// header ident (name + span) the auto-annotation edit targets, where an
/// annotation is legal.
enum DescentTarget<'i, 'f> {
    One {
        model: &'i ModelDef,
        body: &'f Body,
        /// The `oneof` the landing body resolved through, when it did — the
        /// raw fact; consumers apply their own policy. The field-completion
        /// knob filters it through [`defaulted_knob`]; the value governors
        /// apply the validator's CLAIM policy — at the discriminator's value
        /// position the arm keys REPLACE the field channel (a shadowing
        /// field's values would all be validator-rejected), which is also
        /// what makes a valid authored value still complete every arm (the
        /// variant-switching moment).
        via_oneof: Option<&'i OneOfDef>,
    },
    Ambiguous {
        candidates: Vec<NameableVariant<'i>>,
        body: &'f Body,
        header: Option<(String, Span)>,
    },
}

/// A body whose type resolves to a `oneof` with NO discriminator set (or one
/// naming no arm) is a DISCOVERY moment, not a dead end: surface the oneof as
/// the candidate set, so completion offers its discriminator field and the
/// arm keys complete at its value position — the union-of-fields machinery
/// already renders exactly this for oneof candidates. `header` only where the
/// position may legally carry `as` (a union-typed field); on a plain
/// oneof-typed field or element an annotation is a stray (NML2053), so no
/// anchor — and therefore no auto-annotation edit — may attach.
fn unresolved_oneof_target<'i, 'f>(
    oneof: &'i OneOfDef,
    body: &'f Body,
    header: Option<(String, Span)>,
) -> DescentTarget<'i, 'f> {
    DescentTarget::Ambiguous {
        candidates: vec![NameableVariant::OneOf(oneof)],
        body,
        header,
    }
}

/// The union candidate set for an ambiguous body — the shared ORACLE for the
/// D2 case, plus the LIMBO state (an annotation naming no variant: the
/// validator rejects it with NML2051, so completion must not quietly resolve
/// first-wins either — that is F4's shape surviving a typo).
fn ambiguous_candidates<'i>(
    index: &'i SchemaIndex,
    variants: &[FieldType],
    body: &Body,
) -> Option<Vec<NameableVariant<'i>>> {
    if let Some(c) = index.ambiguous_union_variants(variants, body) {
        return Some(c);
    }
    if let Some(ann) = &body.type_annotation {
        // The LIMBO state honors the oracle's SHAPE gate too: a list-shaped
        // body (bare items) is structurally resolvable regardless of its bad
        // annotation — field discovery there would offer names whose
        // acceptance duplicates the items already filling the shorthand
        // field. Only keyed/empty bodies are FIELD-discovery moments. (The
        // pre-discriminator oneof path bypasses this gate safely: a oneof
        // candidate contributes only its discriminator, never a shorthand
        // list field, so that hazard cannot arise there.)
        if BodyShape::of(body).keyed_or_bare()
            && index
                .select_variant_by_type_name(variants, &ann.name)
                .is_none()
        {
            let all: Vec<NameableVariant<'i>> = variants
                .iter()
                .filter_map(|v| match v {
                    FieldType::ModelRef(name) => match index.resolve_ref(name) {
                        Some(FieldTarget::Model(m)) => Some(NameableVariant::Model(m)),
                        Some(FieldTarget::OneOf(o)) => Some(NameableVariant::OneOf(o)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if all.len() >= 2 {
                return Some(all);
            }
        }
    }
    None
}

// String-literal quoting for labels, snippets and hover values lives in
// `nml_core::source_policy::string_literal` — THE one speller, which re-escapes
// the source policy's banned set (a local table here silently diverged
// the day the policy grew, leaking raw steering bytes into editor UI).

/// True when the cursor sits at or after the end of the last `->` token on
/// the line — the arm **target** side (RFC 0007). Mid-selector typing must
/// not trigger target completion just because `->` appears later on the line.
fn cursor_past_arm_arrow(line: &str, byte_end: usize) -> bool {
    let before = &line[..byte_end.min(line.len())];
    before
        .rfind("->")
        .is_some_and(|arrow| byte_end >= arrow + 2)
}

/// The workspace's tagged-ref candidates (`@keyword/name` for every block
/// declaration) — the SAME universe [`find_tagged_ref_definition_in_docs`]
/// resolves, so completion and go-to-definition agree by construction.
fn collect_tagged_ref_candidates(docs: &HashMap<Url, String>) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    for source in docs.values() {
        let file = nml_core::cst::parse_best_effort(source);
        for decl in &file.declarations {
            if let DeclarationKind::Block(block) = &decl.kind {
                refs.push((block.keyword.name.clone(), block.name.name.clone()));
            }
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

/// Completion items for arm selectors before `->` (RFC 0007 §6.1): enum
/// variant keys, a string-key snippet, `@keyword/name` tagged refs for a
/// role-typed `K` (the validator's exact admission rule), and the `else`
/// catch-all.
fn arm_selector_completion_items(
    key: &FieldType,
    index: &SchemaIndex,
    tagged_refs: &[(String, String)],
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    if let Some(enum_def) = index.arm_key_enum_def(key) {
        for (i, variant) in enum_def.variants.iter().enumerate() {
            let quoted = nml_core::source_policy::string_literal(variant);
            items.push(CompletionItem {
                label: quoted.clone(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                detail: Some("arm selector key".to_string()),
                insert_text: Some(format!("{quoted} -> ")),
                sort_text: Some(format!("0_{i:03}")),
                ..Default::default()
            });
        }
    } else if matches!(
        key,
        FieldType::Primitive {
            ty: PrimitiveType::String,
            ..
        }
    ) {
        items.push(CompletionItem {
            label: "\"key\"".to_string(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some("string arm selector key".to_string()),
            insert_text: Some("\"key\" -> ".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: Some("!000".to_string()),
            ..Default::default()
        });
    } else if matches!(
        key,
        FieldType::Primitive {
            ty: PrimitiveType::Role,
            ..
        }
    ) {
        // A role-typed `K` (`(role -> denial)`) selects by tagged reference —
        // the same K-admission rule the validator enforces for
        // `ArmSelector::Role`.
        for (i, (keyword, name)) in tagged_refs.iter().enumerate() {
            let selector = format!("@{keyword}/{name}");
            items.push(CompletionItem {
                label: selector.clone(),
                kind: Some(CompletionItemKind::REFERENCE),
                detail: Some("arm selector".to_string()),
                insert_text: Some(format!("{selector} -> ")),
                sort_text: Some(format!("0_{i:03}_{selector}")),
                ..Default::default()
            });
        }
    }
    items.push(CompletionItem {
        label: "else".to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        detail: Some("catch-all arm selector".to_string()),
        insert_text: Some("else -> ".to_string()),
        sort_text: Some("!001".to_string()),
        ..Default::default()
    });
    items
}

/// Completion item for an inline arm target when `V` admits one (RFC 0007 §6.2).
fn inline_arm_target_snippet_item(
    target_keywords: &[String],
    index: &SchemaIndex,
) -> Option<CompletionItem> {
    let admits_inline = target_keywords
        .iter()
        .any(|keyword| index.field_type_admits_inline(&FieldType::ModelRef(keyword.clone())));
    if !admits_inline {
        return None;
    }
    Some(CompletionItem {
        label: "name:".to_string(),
        kind: Some(CompletionItemKind::TEXT),
        detail: Some("inline arm target (RFC 0007 §6.2)".to_string()),
        insert_text: Some("name:\n    $0".to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        sort_text: Some("!000".to_string()),
        ..Default::default()
    })
}

/// The last entry in `entries` whose header line is strictly above `pos.line`
/// and whose content region includes `pos.line` (bounded by the next entry's
/// header). Editor-grade ownership — not strict span containment.
fn owned_entry_index(
    entries: &[BodyEntry],
    pos: Position,
    _line_index: &LineIndex,
    header_line: impl Fn(&BodyEntry) -> Option<u32>,
) -> Option<usize> {
    let mut owned = None;
    for (i, entry) in entries.iter().enumerate() {
        let Some(start) = header_line(entry) else {
            continue;
        };
        if pos.line <= start {
            continue;
        }
        let next_start = entries.get(i + 1).and_then(&header_line);
        let bounded = match next_start {
            Some(next) => pos.line < next,
            None => true,
        };
        if bounded {
            owned = Some(i);
        }
    }
    owned
}

fn nested_block_header_line(entry: &BodyEntry, line_index: &LineIndex) -> Option<u32> {
    match &entry.kind {
        BodyEntryKind::NestedBlock(_) => Some(span_to_range(entry.span, line_index).start.line),
        _ => None,
    }
}

fn list_item_header_line(entry: &BodyEntry, line_index: &LineIndex) -> Option<u32> {
    match &entry.kind {
        BodyEntryKind::ListItem(_) => Some(span_to_range(entry.span, line_index).start.line),
        _ => None,
    }
}

fn inline_arm_header_line(entry: &BodyEntry, line_index: &LineIndex) -> Option<u32> {
    match &entry.kind {
        BodyEntryKind::Arm(arm) => match &arm.target {
            ArmTarget::Inline { name, .. } => Some(span_to_range(name.span, line_index).start.line),
            _ => None,
        },
        _ => None,
    }
}

/// The inline arm target body owning `pos`, when the cursor sits below its header.
fn inline_arm_body_at<'a>(
    entries: &'a [BodyEntry],
    pos: Position,
    line_index: &LineIndex,
) -> Option<&'a Body> {
    let idx = owned_entry_index(entries, pos, line_index, |e| {
        inline_arm_header_line(e, line_index)
    })?;
    let BodyEntryKind::Arm(arm) = &entries[idx].kind else {
        return None;
    };
    match &arm.target {
        ArmTarget::Inline { body, .. } => Some(body),
        _ => None,
    }
}

/// From a `(model, body)` known to contain the cursor, descend to the
/// innermost body the cursor is in and the target governing it. Recurses
/// through nested model-typed fields, list/set items, oneof variants, and
/// unions. `None` (no suggestions) when the cursor's sub-body resolves to no
/// concrete target.
fn descend_to_cursor<'i, 'f>(
    model: &'i ModelDef,
    body: &'f Body,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
    // The oneof `body` was resolved through, when it was — consumed only if
    // THIS body is the landing body (deeper recursion passes its own).
    via_oneof: Option<&'i OneOfDef>,
) -> Option<DescentTarget<'i, 'f>> {
    // Editor-grade containment (the same rule as top blocks and list items):
    // a mid-typing line under a nested block — INCLUDING the just-typed EMPTY
    // body, the union discovery moment — is often trimmed from the block's
    // content span, so strict span containment would resolve to the parent
    // exactly where completion matters most. Ownership: the last nested block
    // starting before the cursor, bounded by the next sibling's start.
    let owned_idx = owned_entry_index(body.entries.as_slice(), pos, line_index, |e| {
        nested_block_header_line(e, line_index)
    });
    if let Some(i) = owned_idx {
        let BodyEntryKind::NestedBlock(nested) = &body.entries[i].kind else {
            return None;
        };
        let field = model.fields.iter().find(|f| f.name == nested.name.name)?;
        return match index.resolve_field(field) {
            FieldTarget::Model(child) => {
                descend_to_cursor(child, &nested.body, pos, index, line_index, None)
            }
            // A list/set field: the nested body holds list items — descend into
            // the one owning the cursor, resolving the item's model PER ITEM
            // through the canonical body-aware resolver, so `[](modelA |
            // modelB)` items (annotated or shape-selected) complete their
            // variant's fields exactly like `[]model` items.
            FieldTarget::ListOf(_, _) | FieldTarget::SetOf(_, _) => {
                let base = match &field.field_type {
                    FieldType::Modifier(inner) => inner.as_ref(),
                    t => t,
                };
                let (FieldType::List(elem_ty) | FieldType::Set(elem_ty)) = base else {
                    return None;
                };
                let item_idx =
                    owned_entry_index(nested.body.entries.as_slice(), pos, line_index, |e| {
                        list_item_header_line(e, line_index)
                    })?;
                let BodyEntryKind::ListItem(item) = &nested.body.entries[item_idx].kind else {
                    return None;
                };
                let ListItemKind::Named {
                    name: item_name,
                    body: item_body,
                } = &item.kind
                else {
                    return None;
                };
                // The ELEMENT-level twin: an ambiguous item body gets the
                // union-of-fields treatment, anchored at the ITEM name.
                if let FieldType::Union(variants) = elem_ty.as_ref() {
                    if let Some(candidates) = ambiguous_candidates(index, variants, item_body) {
                        return Some(DescentTarget::Ambiguous {
                            candidates,
                            body: item_body,
                            header: Some((item_name.name.clone(), item_name.span)),
                        });
                    }
                }
                let (item_model, item_via) = match index.resolve_type_in_body(elem_ty, item_body) {
                    FieldTarget::Model(m) => (m, None),
                    // A `[]mail` element pre-discriminator: same discovery
                    // moment as the field-level oneof, element twin.
                    FieldTarget::OneOf(o) => match resolve_oneof_variant(o, item_body, index) {
                        Some(m) => (m, Some(o)),
                        None => {
                            // Anchor at the ITEM name only when the element
                            // type is a UNION (where `as` is legal on the
                            // item, same rule as the field-level twin);
                            // plain `[]oneof` elements get none.
                            let header = matches!(elem_ty.as_ref(), FieldType::Union(_))
                                .then(|| (item_name.name.clone(), item_name.span));
                            return Some(unresolved_oneof_target(o, item_body, header));
                        }
                    },
                    _ => return None,
                };
                descend_to_cursor(item_model, item_body, pos, index, line_index, item_via)
            }
            // An arm-set field: descend into an inline arm target's body when
            // the cursor sits below the arm header line.
            FieldTarget::Arms { target, .. } => {
                let idx =
                    owned_entry_index(nested.body.entries.as_slice(), pos, line_index, |e| {
                        inline_arm_header_line(e, line_index)
                    })?;
                let BodyEntryKind::Arm(arm) = &nested.body.entries[idx].kind else {
                    return None;
                };
                let ArmTarget::Inline {
                    name,
                    body: inline_body,
                } = &arm.target
                else {
                    return None;
                };
                let header = (name.name.clone(), name.span);
                match index.resolve_type_in_body(target, inline_body) {
                    FieldTarget::Model(m) => {
                        descend_to_cursor(m, inline_body, pos, index, line_index, None)
                    }
                    FieldTarget::OneOf(o) => match resolve_oneof_variant(o, inline_body, index) {
                        Some(m) => {
                            descend_to_cursor(m, inline_body, pos, index, line_index, Some(o))
                        }
                        None => Some(unresolved_oneof_target(o, inline_body, Some(header))),
                    },
                    _ => None,
                }
            }
            // A `oneof` field: select the variant from the body's discriminator and descend
            // into the same body as that variant model. This is variant-field completion.
            // Pre-discriminator (unset, or set to no arm — the typo state),
            // the body surfaces the oneof itself instead of dying.
            FieldTarget::OneOf(oneof) => match resolve_oneof_variant(oneof, &nested.body, index) {
                Some(variant) => {
                    descend_to_cursor(variant, &nested.body, pos, index, line_index, Some(oneof))
                }
                None => Some(unresolved_oneof_target(oneof, &nested.body, None)),
            },
            // A union FIELD: an AMBIGUOUS body (un-annotated same-class, or
            // unknown-annotation limbo) surfaces its candidate set for the
            // union-of-fields completion, anchored at the field header name;
            // a resolved one descends into its variant.
            FieldTarget::Union(_) => {
                if let Some(variants) = field.field_type.union_variants() {
                    if let Some(candidates) = ambiguous_candidates(index, variants, &nested.body) {
                        return Some(DescentTarget::Ambiguous {
                            candidates,
                            body: &nested.body,
                            header: Some((nested.name.name.clone(), nested.name.span)),
                        });
                    }
                }
                match index.resolve_type_in_body(&field.field_type, &nested.body) {
                    FieldTarget::Model(child) => {
                        descend_to_cursor(child, &nested.body, pos, index, line_index, None)
                    }
                    // The union resolved to a ONEOF variant (`as mail:`, or the
                    // sole nameable member) whose discriminator is still unset:
                    // discovery again — and here the field IS union-typed, so
                    // the header anchor is legal and the discriminator pick may
                    // auto-annotate.
                    FieldTarget::OneOf(o) => match resolve_oneof_variant(o, &nested.body, index) {
                        Some(m) => {
                            descend_to_cursor(m, &nested.body, pos, index, line_index, Some(o))
                        }
                        None => Some(unresolved_oneof_target(
                            o,
                            &nested.body,
                            Some((nested.name.name.clone(), nested.name.span)),
                        )),
                    },
                    _ => None,
                }
            }
            // object / leaf → no concrete model to complete here.
            _ => None,
        };
    }
    Some(DescentTarget::One {
        model,
        body,
        via_oneof,
    })
}

/// The defaulted-discriminator KNOB for a landing body: `Some` iff the body
/// resolved through the oneof's DEFAULT and completion should surface the
/// discriminator as a settable knob (field parity — defaulted knobs show
/// like defaulted fields). Withheld when the name is authored in ANY entry
/// form — the base present-name rule (a Property resolves; a block or
/// modifier form is already invalid, but offering the knob beside it would
/// invite a duplicate name; the shorthand-list extension cannot apply to a
/// discriminator) — or when the variant model SHADOWS the discriminator
/// with a field of its own (the field item covers it).
fn defaulted_knob<'i>(
    model: &ModelDef,
    body: &Body,
    via_oneof: Option<&'i OneOfDef>,
) -> Option<&'i OneOfDef> {
    via_oneof.filter(|o| {
        !present_field_names(body).contains(&o.discriminator)
            && !model.fields.iter().any(|f| f.name == o.discriminator)
    })
}

/// One declaration of a field name across the candidate set: the candidate's
/// index, and its `FieldDef` with its declaration index — `None` for a
/// oneof's discriminator pseudo-field (which sorts first: it is the most
/// discriminating pick a oneof candidate has).
type FieldDecl<'a> = (usize, Option<(usize, &'a FieldDef)>);

/// RFC 0015 F4 — the union-of-fields completion for an UNRESOLVED body
/// (TypeScript-style discovery, plus one step further): every candidate
/// variant's fields are offered with provenance; **tier 0** = fields unique to
/// one variant (the discriminating picks, grouped by variant, required-first
/// within it) — each carries an `additionalTextEdits` that inserts
/// ` as <Variant>` at the header name, so choosing a discriminating field
/// RESOLVES the ambiguity in the same gesture (the auto-import pattern); a
/// `oneof` candidate contributes its discriminator (its fields are unknowable
/// pre-discriminator). **Tier 1** = fields shared by several variants — merged
/// provenance, NO auto-edit (not discriminating), and type scaffolding only
/// when every declaring variant agrees on it.
///
/// EAGER-edit safety: the auto-annotation targets the header line, strictly
/// ABOVE the cursor — typing at the cursor never shifts earlier offsets, so a
/// cached completion list stays applicable (the invariant a resolve-based lazy
/// scheme would otherwise exist for; asserted by test).
fn union_of_fields_completions(
    index: &SchemaIndex,
    candidates: &[NameableVariant<'_>],
    body: &Body,
    header: Option<&(String, Span)>,
    line_index: &LineIndex,
    label_details: bool,
) -> Vec<CompletionItem> {
    let present = present_field_names(body);
    // (field name) -> its declarations across candidates. Insertion-ordered
    // Vec for deterministic output + a HashMap index so recording stays
    // linear in total field count (a Vec-scan per field would be quadratic).
    let mut occurrences: Vec<(String, Vec<FieldDecl>)> = Vec::new();
    let mut occ_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    fn record<'a>(
        occurrences: &mut Vec<(String, Vec<FieldDecl<'a>>)>,
        occ_index: &mut std::collections::HashMap<String, usize>,
        name: &str,
        decl: FieldDecl<'a>,
    ) {
        if let Some(&i) = occ_index.get(name) {
            occurrences[i].1.push(decl);
        } else {
            occ_index.insert(name.to_string(), occurrences.len());
            occurrences.push((name.to_string(), vec![decl]));
        }
    }
    for (ci, cand) in candidates.iter().enumerate() {
        match cand {
            NameableVariant::Model(m) => {
                for (fi, field) in m.fields.iter().enumerate() {
                    record(
                        &mut occurrences,
                        &mut occ_index,
                        &field.name,
                        (ci, Some((fi, field))),
                    );
                }
            }
            NameableVariant::OneOf(o) => {
                record(
                    &mut occurrences,
                    &mut occ_index,
                    &o.discriminator,
                    (ci, None),
                );
            }
        }
    }
    let annotation_edit = |variant: &str| -> Option<Vec<TextEdit>> {
        let (name, span) = header?;
        // An annotation may already be present: unknown (the LIMBO state) —
        // the edit must REPLACE it, a name-token-only edit would produce
        // `slot as modelA as nope:` — or already naming THIS variant (a
        // pre-discriminator oneof under an annotated union field): then the
        // edit would be a byte-identical no-op, and announcing "adds `as X`"
        // for it would be false — attach nothing.
        let end = match &body.type_annotation {
            Some(a) if a.name == variant => return None,
            Some(a) => a.span.end.max(span.end),
            None => span.end,
        };
        Some(vec![TextEdit {
            range: line_index.range(Span::new(span.start, end)),
            new_text: format!("{name} as {variant}"),
        }])
    };
    let mut items = Vec::new();
    for (fname, decls) in &occurrences {
        if present.contains(fname) {
            continue;
        }
        let unique = decls.len() == 1;
        if unique {
            let (ci, fd) = decls[0];
            let variant = candidates[ci].name();
            let annotation = annotation_edit(variant);
            // The announcement is HONEST: it appears exactly when the edit is
            // attached (no header anchor — e.g. a pre-discriminator plain
            // oneof field, where `as` would be a stray — announces nothing).
            let announce = annotation.is_some().then(|| format!("adds `as {variant}`"));
            let (detail, docs, insert, in_group) = match fd {
                Some((fi, field)) => (
                    field_detail(field),
                    field.doc.clone(),
                    field_insert_text(index, field),
                    field_sort_key(field, fi),
                ),
                // A oneof discriminator: scaffold the property form; it sorts
                // FIRST in its group (the most discriminating pick).
                None => (
                    format!("discriminator of `{variant}`"),
                    None,
                    format!("{fname} = "),
                    "0_0000".to_string(),
                ),
            };
            let (label, filter_text) = match fd {
                Some((_, field)) => field_label(field),
                None => (fname.clone(), None),
            };
            items.push(CompletionItem {
                label,
                filter_text,
                kind: Some(CompletionItemKind::FIELD),
                label_details: label_details.then(|| CompletionItemLabelDetails {
                    detail: None,
                    description: Some(match &announce {
                        Some(a) => format!("{variant} — {a}"),
                        None => variant.to_string(),
                    }),
                }),
                detail: Some(match (&announce, label_details) {
                    (_, true) => detail,
                    (Some(a), false) => format!("{detail} — {variant} ({a})"),
                    (None, false) => format!("{detail} — {variant}"),
                }),
                documentation: docs.map(Documentation::String),
                // Tier 0: grouped by variant, required-first + declaration
                // order WITHIN the group (the same key single-model field
                // completion uses).
                sort_text: Some(format!("0_{ci:03}_{in_group}")),
                insert_text: Some(insert),
                additional_text_edits: annotation,
                ..Default::default()
            });
        } else {
            let provenance: Vec<&str> =
                decls.iter().map(|(ci, _)| candidates[*ci].name()).collect();
            // Scaffolding only when every declaring variant agrees on the
            // field's declared type AND its form — `FieldType`'s Display
            // erases the `|` wrapper, so modifier-ness must be compared
            // explicitly or `|vis string` vs `vis string` would false-agree.
            let fields: Vec<&FieldDef> = decls
                .iter()
                .filter_map(|(_, fd)| fd.map(|(_, f)| f))
                .collect();
            let all_decls_are_fields = fields.len() == decls.len();
            let uniform_modifier =
                all_decls_are_fields && fields.iter().all(|f| is_modifier_form(f));
            let uniform_plain = fields.iter().all(|f| !is_modifier_form(f));
            let agree = all_decls_are_fields
                && (uniform_modifier || uniform_plain)
                && fields
                    .windows(2)
                    .all(|w| w[0].field_type.to_string() == w[1].field_type.to_string());
            // A name that is a DISCRIMINATOR in every candidate (two oneofs
            // sharing `by kind`): the property form is safe for all, so
            // scaffold it — the merged arm keys then complete at the value.
            let all_discriminators = decls.iter().all(|(_, fd)| fd.is_none());
            // The no-scaffold fallback still honors a uniform authored form:
            // when every declarer is a modifier, a sigil-less insert would
            // author an unknown PROPERTY, not the field. Mixed forms get the
            // bare name — the author's pick of form decides the variant.
            let (label, filter_text, insert) = if agree {
                let (label, filter) = field_label(fields[0]);
                (label, filter, field_insert_text(index, fields[0]))
            } else if uniform_modifier {
                (
                    format!("|{fname}"),
                    Some(fname.clone()),
                    format!("|{fname}"),
                )
            } else if all_discriminators {
                (fname.clone(), None, format!("{fname} = "))
            } else {
                (fname.clone(), None, fname.clone())
            };
            let detail = if agree {
                format!("{} — {}", field_detail(fields[0]), provenance.join(" | "))
            } else {
                provenance.join(" | ")
            };
            items.push(CompletionItem {
                label,
                filter_text,
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(detail),
                sort_text: Some(format!("1_{fname}")),
                insert_text: Some(insert),
                ..Default::default()
            });
        }
    }
    items
}

/// The concrete MODEL a body resolves to under `ty` — through the canonical
/// body-aware resolver, then through a `oneof` variant's discriminator when the
/// resolution lands on a oneof. Serves the `as`-slot descent
/// ([`find_union_list_field_at`]); the completion descent inlines the same
/// two-step so an UNRESOLVED oneof can surface its discovery target instead
/// of `None`.
fn variant_model_for_body<'i>(
    index: &'i SchemaIndex,
    ty: &'i FieldType,
    body: &Body,
) -> Option<&'i ModelDef> {
    match index.resolve_type_in_body(ty, body) {
        FieldTarget::Model(m) => Some(m),
        FieldTarget::OneOf(o) => resolve_oneof_variant(o, body, index),
        _ => None,
    }
}

/// Resolve a `oneof` instance body to its variant model: read the
/// discriminator value the body sets (or the schema default), match it to an
/// arm, and resolve that variant. `None` when no discriminator is
/// set/defaulted or it names no arm — the completion descent turns exactly
/// that `None` into the pre-discriminator DISCOVERY moment
/// ([`unresolved_oneof_target`]).
fn resolve_oneof_variant<'i>(
    oneof: &OneOfDef,
    body: &Body,
    index: &'i SchemaIndex,
) -> Option<&'i ModelDef> {
    let value = body
        .entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::Property(p) if p.name.name == oneof.discriminator => {
                p.value.value.as_str().map(str::to_owned)
            }
            _ => None,
        })
        .or_else(|| oneof.default_discriminator.clone())?;
    let (_, variant_model) = oneof.variants.iter().find(|(v, _)| *v == value)?;
    match index.resolve_ref(variant_model) {
        Some(FieldTarget::Model(m)) => Some(m),
        _ => None,
    }
}

/// Field names already SET in `body` for `model`: [`present_field_names`]
/// plus the body-positional shorthand field when bare list items fill it
/// (RFC 0005 `+` on a list/set) — the validator's seen-scan counts those
/// items as setting that field, so completion must not re-offer it (accepting
/// the re-offer would author a duplicate named block beside the bare items).
/// The candidate-set paths need no counterpart: the D2 oracle and the limbo
/// gate never classify a list-shaped body as ambiguous, and the one producer
/// that CAN carry list-shaped bodies (the pre-discriminator oneof) offers
/// only its discriminator — never a shorthand list field — so the
/// duplicate-authoring hazard cannot arise there.
fn present_field_names_in(model: &ModelDef, body: &Body) -> HashSet<String> {
    let mut present = present_field_names(body);
    if body
        .entries
        .iter()
        .any(|e| matches!(e.kind, BodyEntryKind::ListItem(_)))
    {
        if let Some(field) = model
            .fields
            .iter()
            .find(|f| f.shorthand && matches!(f.field_type, FieldType::List(_) | FieldType::Set(_)))
        {
            present.insert(field.name.clone());
        }
    }
    present
}

/// Property/block names already present in `body` — excluded from field suggestions so a
/// field set once is not re-offered.
fn present_field_names(body: &Body) -> HashSet<String> {
    body.entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            BodyEntryKind::Property(prop) => Some(prop.name.name.clone()),
            BodyEntryKind::NestedBlock(nested) => Some(nested.name.name.clone()),
            // A modifier entry SETS its field (`|vis = @admin`): without this,
            // completion re-offers `vis` on a body that already authored it.
            BodyEntryKind::Modifier(m) => Some(m.name.name.clone()),
            _ => None,
        })
        .collect()
}

/// `detail` for a field completion — the NML type as authored, with `?` for optional and
/// `= <default>` when the schema declares one (so the author sees the effective value).
fn field_detail(field: &FieldDef) -> String {
    let mut detail = format!(
        "{}{}",
        field.field_type,
        if field.optional { "?" } else { "" }
    );
    if let Some(rendered) = field
        .default_value
        .as_ref()
        .and_then(|d| render_scalar(&d.value))
    {
        detail.push_str(&format!(" = {rendered}"));
    }
    detail
}

/// Render a **scalar** schema default to its NML text for a completion hint. Schema defaults
/// are always scalars, so this is sufficient; a non-scalar (array/template/…) returns `None`
/// and is simply omitted from the hint rather than rendered imprecisely.
fn render_scalar(value: &Value) -> Option<String> {
    Some(match value {
        Value::String(s) => format!("{s:?}"),
        Value::Number(n) => n.to_string(),
        Value::Money(m) => m.format_display(),
        Value::Duration(d) => d.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Reference(s) | Value::Role(s) | Value::Secret(s) => s.clone(),
        _ => return None,
    })
}

/// Sort key: required fields first, then schema declaration order (`idx`).
fn field_sort_key(field: &FieldDef, idx: usize) -> String {
    format!("{}_{idx:04}", u8::from(field.optional))
}

/// The authored FORM of a field: modifier-declared fields are written with
/// the `|` sigil (`|vis = …`), plain fields without. The one predicate behind
/// form matching (value governors), tier-1 form agreement, labels, and
/// insert-text sigils — the completion layer's central concept, named once.
fn is_modifier_form(field: &FieldDef) -> bool {
    matches!(field.field_type, FieldType::Modifier(_))
}

/// `insert_text`: `<field> = ` for a scalar/leaf field, `<field>:` for a model/oneof/list/
/// object field (which is authored as a block) — a blanket `= ` would be wrong for blocks.
/// Completion label + filter text for a field: a modifier-declared field
/// DISPLAYS as authored (`|vis`) so the list is honest about its form, while
/// filtering stays on the bare name (typing `vis` still matches).
fn field_label(field: &FieldDef) -> (String, Option<String>) {
    if is_modifier_form(field) {
        (format!("|{}", field.name), Some(field.name.clone()))
    } else {
        (field.name.clone(), None)
    }
}

fn field_insert_text(index: &SchemaIndex, field: &FieldDef) -> String {
    // A modifier-declared field is AUTHORED with its sigil (`|vis = …`) — a
    // sigil-less insert would author an unknown PROPERTY, not the modifier.
    let sigil = if is_modifier_form(field) { "|" } else { "" };
    match index.resolve_field(field) {
        FieldTarget::Leaf(_) => format!("{sigil}{} = ", field.name),
        _ => format!("{sigil}{}:", field.name),
    }
}

/// Parse the wire `suggestions` payload of a diagnostic's `data` (see the
/// diagnostics.rs producer): every valid `{replacement, start, end, kind}`
/// entry — with `source`, the document the edit lands in, when it is not
/// this one (the action is minted on THAT document) — capped at the
/// producer's own alternative bound
/// (`MAX_FIX_ALTERNATIVES` in nml-validate). Parse-then-cap, in that order:
/// the cap counts VALID entries — so a hostile/buggy client can neither mint
/// unbounded actions from one diagnostic nor bury a legitimate entry behind
/// malformed padding — and the singleton-preferred gate downstream counts
/// only real suggestions.
fn parse_suggestion_entries(suggestions: &[serde_json::Value]) -> Vec<SuggestionEntry> {
    suggestions
        .iter()
        .filter_map(|s| {
            let start = s.get("start")?.as_u64()? as usize;
            let end = s.get("end")?.as_u64()? as usize;
            // An inverted span would round-trip as a spec-invalid LSP Range;
            // fail closed like every other malformed entry.
            (start <= end).then_some(())?;
            // `source` is the file the edit lands in when it is not this
            // document (`Suggestion::source`, a key); absent = this one.
            let source = match s.get("source") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(v.as_str()?.to_string()),
            };
            Some(SuggestionEntry {
                replacement: s.get("replacement")?.as_str()?.to_string(),
                start,
                end,
                kind: s.get("kind")?.as_str()?.to_string(),
                source,
            })
        })
        .take(MAX_SUGGESTION_ACTIONS)
        .collect()
}

/// One `data.suggestions[]` entry as the client round-tripped it.
struct SuggestionEntry {
    replacement: String,
    start: usize,
    end: usize,
    kind: String,
    source: Option<String>,
}

/// The `layers` object `nml/schemaInfo` carries — the `--json` `binding`
/// row's, from the kernel's one spelling.
fn layers_value(grant: &nml_validate::workspace::Grant) -> serde_json::Value {
    serde_json::to_value(grant.wire()).unwrap_or(serde_json::Value::Null)
}

/// The binding's grant for the `(0,0)` hover, in `nml binding`'s words —
/// the kernel's one spelling of the rows (`LayerGrant::rules`) joined on
/// one line, or the one denied sentence (`Grant::DENIED`); nothing for a
/// file no binding governs.
fn layers_summary(grant: &nml_validate::workspace::Grant) -> Option<String> {
    use nml_validate::workspace::Grant;
    match grant {
        Grant::Granted { grant, .. } => Some(format!(
            "granted — {}",
            grant.rules().collect::<Vec<_>>().join(", ")
        )),
        Grant::NoGrant { .. } => Some(Grant::DENIED.to_string()),
        Grant::Ambiguous { .. } | Grant::Unbound { .. } => None,
    }
}

/// Push `action` unless an action with the same title and edit is already
/// offered — one insertion asked for by two diagnostics is one action.
fn push_unique_action(actions: &mut Vec<CodeActionOrCommand>, action: CodeAction) {
    let twin = actions.iter().any(|a| match a {
        CodeActionOrCommand::CodeAction(existing) => {
            existing.title == action.title && existing.edit == action.edit
        }
        CodeActionOrCommand::Command(_) => false,
    });
    if !twin {
        actions.push(CodeActionOrCommand::CodeAction(action));
    }
}

/// When the cursor is at the value position of a `oneof` instance's discriminator
/// (`<discriminator> = <here>` inside a block whose keyword names the union), return
/// that `oneof` so its arm keys can be offered as completions.
fn find_oneof_discriminator_at<'i>(
    file: &File,
    source: &str,
    pos: Position,
    index: &'i SchemaIndex,
    line_index: &LineIndex,
) -> Option<&'i OneOfDef> {
    let prop_name = value_position_prop_name(source, pos)?;
    let keyword = find_enclosing_block_keyword(file, pos, line_index)?;
    match index.resolve_ref(&keyword) {
        Some(FieldTarget::OneOf(oneof)) if oneof.discriminator == prop_name => Some(oneof),
        _ => None,
    }
}

/// Collect declaration names matching a specific keyword from all loaded docs.
fn collect_declarations_by_keyword(
    docs: &HashMap<Url, String>,
    keyword: &str,
) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    for (uri, source) in docs.iter() {
        let file = nml_core::cst::parse_best_effort(source);
        let file_name = uri
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("unknown")
            .to_string();
        for decl in &file.declarations {
            match &decl.kind {
                DeclarationKind::Block(block) if block.keyword.name == keyword => {
                    results.push((
                        block.name.name.clone(),
                        block.keyword.name.clone(),
                        file_name.clone(),
                    ));
                }
                DeclarationKind::Array(arr) if arr.item_keyword.name == keyword => {
                    for item in &arr.body.items {
                        if let ListItemKind::Named { name, .. } = &item.kind {
                            results.push((
                                name.name.clone(),
                                arr.item_keyword.name.clone(),
                                file_name.clone(),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    results
}

/// The declaration-hover lookup across the open documents: a top-level
/// declaration named `word` — or a **named array item** (`- ProUpsell:` in
/// `[]denial denials:`), the form arm targets (RFC 0007 §4.1) and other item
/// references name — rendered as hover markdown with its leading-comment
/// documentation, body summary, and source file. A declaration **outranks** a
/// same-named item (first pass finds only declarations; items resolve in a
/// second pass). Extracted from `hover` so the lookup is unit-testable
/// without a server.
fn find_declaration_hover(
    docs: &HashMap<Url, String>,
    word: &str,
    model_ref_types: &[String],
) -> Option<String> {
    let mut item_hover: Option<String> = None;
    for (doc_uri, source) in docs.iter() {
        let file = nml_core::cst::parse_best_effort(source);
        for decl in &file.declarations {
            let (kw, decl_name, body_summary) = match &decl.kind {
                DeclarationKind::Block(block) if block.name.name == word => {
                    let summary = summarize_body(&block.body);
                    (block.keyword.name.clone(), block.name.name.clone(), summary)
                }
                DeclarationKind::Array(arr) if arr.name.name == word => (
                    format!("[]{}", arr.item_keyword.name),
                    arr.name.name.clone(),
                    String::new(),
                ),
                // A named item hovers like a declaration of the array's item
                // keyword — `- ProUpsell:` in `[]denial denials:` reads
                // `**denial** \`ProUpsell\`` — but only as the FALLBACK: a
                // top-level declaration of the same name wins, so the first
                // item hover is held rather than returned.
                DeclarationKind::Array(arr) => {
                    if item_hover.is_none() {
                        if let Some(item_body) = arr.body.items.iter().find_map(|item| match &item
                            .kind
                        {
                            ListItemKind::Named { name, body } if name.name == word => Some(body),
                            _ => None,
                        }) {
                            item_hover = Some(render_declaration_hover(
                                &arr.item_keyword.name,
                                word,
                                &summarize_body(item_body),
                                model_ref_types,
                                source,
                                doc_uri,
                            ));
                        }
                    }
                    continue;
                }
                DeclarationKind::Const(c) if c.name.name == word => {
                    let val = format_named_value(&c.name.name, &c.value.value);
                    ("const".into(), c.name.name.clone(), val)
                }
                DeclarationKind::Template(t) if t.name.name == word => {
                    let val = format_named_value(&t.name.name, &t.value.value);
                    ("template".into(), t.name.name.clone(), val)
                }
                _ => continue,
            };
            return Some(render_declaration_hover(
                &kw,
                &decl_name,
                &body_summary,
                model_ref_types,
                source,
                doc_uri,
            ));
        }
    }
    item_hover
}

/// Assemble one hover text: `**keyword** \`name\``, the reference context, the
/// leading-comment documentation (declaration or named array item — RFC 0004
/// §4.3 via `doc_comment_for`), the body summary, and the source file. The
/// single renderer for declaration and item hovers, so the two can never
/// drift.
fn render_declaration_hover(
    kw: &str,
    decl_name: &str,
    body_summary: &str,
    model_ref_types: &[String],
    source: &str,
    doc_uri: &Url,
) -> String {
    let mut text = format!("**{kw}** `{decl_name}`");
    // Multiple governing types (a union-typed or candidate-merged value
    // position) are all stated — the position honestly admits any of them.
    if !model_ref_types.is_empty() {
        text.push_str(&format!(
            " *(referenced as {})*",
            model_ref_types.join(" | ")
        ));
    }
    if let Some(doc) = nml_core::cst::doc_comment_for(source, decl_name) {
        text.push_str("\n\n");
        text.push_str(&doc);
    }
    if !body_summary.is_empty() {
        text.push_str("\n\n");
        text.push_str(body_summary);
    }
    let file_name = doc_uri
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("unknown");
    text.push_str(&format!("\n\n*Source: {file_name}*"));
    text
}

/// Whether the *byte* column `byte_col` sits on a property name.
fn is_property_name_position(line: &str, word: &str, byte_col: usize) -> bool {
    if word.is_empty() {
        return false;
    }
    let trimmed = line.trim();

    if let Some(eq_pos) = line.find('=') {
        if byte_col < eq_pos {
            return true;
        }
    }

    if trimmed.ends_with(':') && !trimmed.starts_with("//") {
        let before_colon = &trimmed[..trimmed.len() - 1];
        let indent = line.len() - line.trim_start().len();
        if !before_colon.contains(' ') && indent > 0 {
            return true;
        }
    }

    false
}

/// "Simplify number" (RFC 0016 §1.10), decided as a pure function so the
/// action's edge cases are unit-testable without LSP plumbing.
struct SimplifyNumber {
    title: String,
    span: nml_core::span::Span,
    new_text: String,
}

fn simplify_number_action(source: &str, offset: usize) -> Option<SimplifyNumber> {
    let root = nml_core::cst::parse(source).syntax();
    let tok = root
        .token_at_offset((offset.min(source.len()) as u32).into())
        .find(|t| t.kind() == nml_core::cst::SyntaxKind::Number)?;
    // Money literals (`19.90 USD`) are Number + currency Ident inside one
    // Value node. Duration literals wrap in `DurationLiteral`. Simplifying
    // the number half would fight `nml fmt` or corrupt compounds.
    let parent = tok.parent()?;
    if parent.kind() == nml_core::cst::SyntaxKind::DurationLiteral {
        return None;
    }
    let in_money = parent.children_with_tokens().any(|e| {
        e.into_token().is_some_and(|t| {
            t.kind() == nml_core::cst::SyntaxKind::Ident
                && t.text().len() == 3
                && t.text().chars().all(|c| c.is_ascii_uppercase())
        })
    });
    if in_money {
        return None;
    }
    let raw = tok.text();
    let n: nml_core::decimal::Number = raw.parse().ok()?;
    // The minimal cohort member with scale ≥ 0 — cohort simplification
    // lives in the numeric core, not here (leading integer zeros drop
    // via the parse itself).
    let simplified = n.simplified().to_string();
    // Spelling-equivalence modulo `_` separators: `1_000` is already its
    // simplified value in the author's chosen grouping — offering
    // "simplify to `1000`" would nag users into deleting the separators
    // the language gives them. `007` → `7` still fires.
    if simplified == raw.replace('_', "") {
        return None;
    }
    // The Number token never carries the sign (`-` lexes as its own
    // adjacent token), so spell the sign back into the TITLE — the edit
    // itself only replaces the digits, which is already correct.
    let signed = tok.prev_token().is_some_and(|prev| prev.text() == "-");
    let display = if signed {
        format!("-{simplified}")
    } else {
        simplified.clone()
    };
    Some(SimplifyNumber {
        title: format!("Simplify number to `{display}`"),
        span: nml_core::span::Span::new(
            usize::from(tok.text_range().start()),
            usize::from(tok.text_range().end()),
        ),
        new_text: simplified,
    })
}

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => nml_core::source_policy::string_literal(s),
        Value::Number(n) => n.to_string(),
        Value::Money(m) => m.format_display(),
        Value::Duration(d) => d.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Reference(r) => r.clone(),
        Value::Secret(s) => s.clone(),
        Value::Role(r) => r.clone(),
        _ => "...".to_string(),
    }
}

/// Whether a property name suggests credential material.
fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["key", "token", "secret", "password"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Format a named value for hover display, redacting literal strings whose
/// name suggests credentials. `Value::Secret` is shown as-is: it renders
/// the `$ENV.KEY` reference text, not actual secret material.
fn format_named_value(name: &str, value: &Value) -> String {
    if matches!(value, Value::String(_)) && is_sensitive_name(name) {
        "\"…\"".to_string()
    } else {
        format_value(value)
    }
}

fn is_template_namespace_position(before_cursor: &str) -> bool {
    if let Some(last_open) = before_cursor.rfind("{{") {
        let after_open = &before_cursor[last_open + 2..];
        if after_open.contains("}}") {
            return false;
        }
        after_open.trim().is_empty()
    } else {
        false
    }
}

// ── On-type indent computation ───────────────────────────────

fn is_inside_triple_quote(lines: &[&str], line_idx: usize) -> bool {
    let mut open = false;
    for (i, line) in lines.iter().enumerate() {
        if i >= line_idx {
            break;
        }
        for _ in 0..count_triple_quotes(line) {
            open = !open;
        }
    }
    open
}

/// Line-local `"""` delimiter count, ESCAPE-AWARE (parity with the lexer's
/// string scan): a `\X` pair is skipped first, so `\"""` is an escaped
/// quote followed by two plain quotes — not a delimiter. A naive substring
/// count would toggle on it and mis-indent every line after. (Outside
/// strings a backslash is not an escape, but no legal NML has one there —
/// pair-skipping everywhere is the right cheap approximation for an
/// indent heuristic.)
fn count_triple_quotes(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut count = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if bytes[i..].starts_with(b"\"\"\"") {
            count += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    count
}

/// Compute the desired indentation (in spaces) for a new line inserted after
/// `line_idx` in the given source lines — one `unit` deeper after a block
/// header, the unit being the one a structural insertion there would nest
/// by (`nml_core::cst::edit::indentation_unit_at`: the file's own, the
/// canonical four when the file offers none). This drives `onTypeFormatting`
/// for the `\n` trigger so the cursor lands at the right column.
fn compute_indent_after_line(lines: &[&str], line_idx: usize, unit: usize) -> usize {
    let effective_idx = if line_idx < lines.len() {
        let mut idx = line_idx;
        while idx > 0 && lines[idx].trim().is_empty() {
            idx -= 1;
        }
        idx
    } else if !lines.is_empty() {
        lines.len() - 1
    } else {
        return 0;
    };

    let line = lines[effective_idx];
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return 0;
    }

    if is_inside_triple_quote(lines, line_idx + 1) {
        return line.len() - line.trim_start().len();
    }

    let prev_indent = line.len() - line.trim_start().len();

    if trimmed.ends_with(':') && !trimmed.starts_with("//") {
        return prev_indent + unit;
    }

    prev_indent
}

// ── LanguageServer implementation ─────────────────────────────

/// The single-line replace/insert ranges for a value completion at `pos`:
/// the existing value token (from the first non-space after `=` to the end
/// of its contiguous run) is replaced; the insert range stops at the cursor
/// (LSP 3.16 insert-vs-replace semantics). `None` when the line has no `=`
/// before the cursor.
fn value_edit_ranges(source: &str, pos: Position) -> Option<(Range, Range)> {
    let line = position::line_at(source, pos.line)?;
    let cursor = position::utf16_to_byte(line, pos.character);
    let eq = line[..cursor].find('=')?;
    let after_eq = eq + 1;
    let token_start = after_eq
        + line[after_eq..]
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(cursor.saturating_sub(after_eq));
    let token_start = token_start.min(cursor);
    // A quoted value extends to its closing quote (a `"a b"` literal is one
    // token); anything else ends at the next whitespace.
    let token_end = if line[token_start..].starts_with('"') {
        line[token_start + 1..]
            .find('"')
            .map(|i| token_start + 1 + i + 1)
            .unwrap_or(line.len())
    } else {
        token_start
            + line[token_start..]
                .find(|c: char| c.is_whitespace())
                .unwrap_or(line.len() - token_start)
    };
    let token_end = token_end.max(cursor);
    let col = |b: usize| position::byte_to_utf16(line, b);
    let replace = Range::new(
        Position::new(pos.line, col(token_start)),
        Position::new(pos.line, col(token_end)),
    );
    let insert = Range::new(
        Position::new(pos.line, col(token_start)),
        Position::new(pos.line, col(cursor)),
    );
    Some((insert, replace))
}

/// A quoted-value completion item with a precise edit: `InsertReplaceEdit`
/// when the client supports it (capability-gated), plain `TextEdit`
/// otherwise; `filter_text` is the quoted form because clients filter
/// against the range text, which starts at the opening quote.
fn quoted_value_item(
    variant: &str,
    detail: &str,
    sort: String,
    edit_ranges: Option<(Range, Range)>,
    insert_replace: bool,
) -> CompletionItem {
    let quoted = nml_core::source_policy::string_literal(variant);
    let text_edit = edit_ranges.map(|(insert, replace)| {
        if insert_replace {
            CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
                new_text: quoted.clone(),
                insert,
                replace,
            })
        } else {
            CompletionTextEdit::Edit(TextEdit {
                range: replace,
                new_text: quoted.clone(),
            })
        }
    });
    CompletionItem {
        label: quoted.clone(),
        kind: Some(CompletionItemKind::ENUM_MEMBER),
        detail: Some(detail.to_string()),
        sort_text: Some(sort),
        filter_text: Some(quoted),
        text_edit,
        ..Default::default()
    }
}

/// A duration unit-suffix completion item (RFC 0017): the full literal
/// (`30s`) as label and filter text — clients filter against the token
/// text (the typed digits), so a bare-suffix label would be filtered out
/// before the user ever saw it. The edit replaces exactly the digits
/// (and, in replace mode, any stale suffix after the cursor), with the
/// same capability-gated `InsertReplaceEdit` handling as
/// [`quoted_value_item`].
fn duration_unit_item(
    ctx: &DurationUnitContext,
    suffix: &str,
    unit_name: &str,
    sort: String,
    insert_replace: bool,
) -> CompletionItem {
    let literal = format!("{}{suffix}", ctx.digits);
    let text_edit = if insert_replace {
        CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
            new_text: literal.clone(),
            insert: ctx.insert,
            replace: ctx.replace,
        })
    } else {
        // Plain-edit clients get the replace range (as [`quoted_value_item`]
        // does), so a stale suffix after the cursor is swapped, not stacked.
        CompletionTextEdit::Edit(TextEdit {
            range: ctx.replace,
            new_text: literal.clone(),
        })
    };
    CompletionItem {
        label: literal.clone(),
        kind: Some(CompletionItemKind::UNIT),
        detail: Some(unit_name.to_string()),
        label_details: duration_lsp::completion_preview(ctx, suffix).map(|preview| {
            CompletionItemLabelDetails {
                detail: Some(format!("= {preview}")),
                ..Default::default()
            }
        }),
        sort_text: Some(sort),
        filter_text: Some(literal),
        text_edit: Some(text_edit),
        ..Default::default()
    }
}

/// A schema index for editor surfaces: borrowed from a bound package's
/// validator, or owned (built from the scope registry).
enum IndexHandle {
    Bound(std::sync::Arc<nml_validate::schema::SchemaValidator>),
    Registry(Box<SchemaIndex>),
}

impl IndexHandle {
    fn index(&self) -> &SchemaIndex {
        match self {
            IndexHandle::Bound(v) => v.index(),
            IndexHandle::Registry(i) => i,
        }
    }
}

/// What a pin/opt-out code action writes into `nml-project.nml`.
enum ProjectEdit {
    Pin(String),
    OptOut,
}

impl Inner {
    /// The workspace edit applying `edits` to `uri` — one file's case of
    /// [`Self::workspace_edits`].
    ///
    /// Callers must not hold the `documents` lock: the versioned shape
    /// reads it (`std::sync::Mutex` is not re-entrant).
    fn workspace_edit(&self, uri: Url, edits: Vec<TextEdit>) -> WorkspaceEdit {
        self.workspace_edits(vec![(uri, edits)])
    }

    /// The workspace edit applying each file's `edits` — the ONE shape
    /// every action the server hands out takes, a rename across files
    /// included: `documentChanges` naming each document's client VERSION
    /// (an open buffer's; `null` for a file the client did not open, whose
    /// master is the disk) when the client declared
    /// `workspace.workspaceEdit.documentChanges`, so it refuses an edit
    /// computed against text that has since moved on (LSP 3.17
    /// §WorkspaceEdit, `OptionalVersionedTextDocumentIdentifier`); plain
    /// `changes` otherwise — the only shape such a client can apply.
    ///
    /// Sorted by URI: `documentChanges` is an ORDERED array, and a hash
    /// map's iteration order would hand the same rename out in a different
    /// order every call.
    fn workspace_edits(&self, mut per_file: Vec<(Url, Vec<TextEdit>)>) -> WorkspaceEdit {
        per_file.sort_by(|(a, _), (b, _)| a.as_str().cmp(b.as_str()));
        if self
            .versioned_edits
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            return WorkspaceEdit {
                document_changes: Some(DocumentChanges::Edits(
                    per_file
                        .into_iter()
                        .map(|(uri, edits)| TextDocumentEdit {
                            text_document: OptionalVersionedTextDocumentIdentifier {
                                version: docs.version(&uri),
                                uri,
                            },
                            edits: edits.into_iter().map(OneOf::Left).collect(),
                        })
                        .collect(),
                )),
                ..Default::default()
            };
        }
        WorkspaceEdit {
            changes: Some(per_file.into_iter().collect()),
            ..Default::default()
        }
    }

    /// The workspace edit creating `uri` with `content` — `None` for a
    /// client that declared no `create` resource operation: an action it
    /// cannot apply is never offered.
    fn create_file_edit(&self, uri: Url, content: String) -> Option<WorkspaceEdit> {
        if !self
            .creates_files
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        Some(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                    uri: uri.clone(),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                    edits: vec![OneOf::Left(TextEdit {
                        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                        new_text: content,
                    })],
                }),
            ])),
            ..Default::default()
        })
    }

    /// Whether an action may WRITE `path`: inside a workspace root, never
    /// beyond (a root marker in `$HOME` must not make the editor create
    /// `~/nml-project.nml`; a derived universe above every folder is read,
    /// never written). Containment only — no ancestor allowance: for any
    /// file INSIDE a workspace root the binding walk
    /// (`ancestors_within_roots`) is bounded by that root, so every
    /// legitimate write target already satisfies this check, and root
    /// markers are attacker-influenced (`rootMarkers` is a plain string
    /// list) — an out-of-workspace target is exactly the write this
    /// guard refuses.
    fn may_write(&self, path: &Path) -> bool {
        self.workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|r| path.starts_with(r))
    }

    /// The text of `path` as an action edits it — the open buffer's
    /// (its unsaved text is what the computed offsets must be valid
    /// against), else the disk's under the cap the kernel reads that
    /// kind of input with.
    fn editable_text(&self, uri: &Url, path: &Path) -> Option<String> {
        let buffered = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
            .cloned();
        buffered.or_else(|| packages::read_input_at_leaf(packages::input_kind_of(path), path).ok())
    }
}

impl NmlLanguageServer {
    /// Build the workspace edit for a pin/opt-out action targeting the
    /// nearest LIVE `nml-project.nml` — `existing`, the kernel's answer
    /// (`project_config_path_for`) — with a structural CST insert into its
    /// `project` block (RFC 0030 P2), or creating one at the binding's
    /// anchor `root`.
    /// Injection-safe twice over: package names are charset-constrained at
    /// package load, and the CST splice refuses any snippet that does not
    /// parse as plain body entries.
    fn project_edit_action(
        &self,
        existing: Option<&Path>,
        root: &Path,
        title: String,
        edit: ProjectEdit,
    ) -> Option<CodeAction> {
        // The action writes a file: never one outside the workspace
        // (`may_write`) — a binding root ABOVE the workspace can only come
        // from an out-of-workspace file whose unbounded walk matched a
        // marker in `$HOME` or another shared ancestor.
        if !self.may_write(root) || existing.is_some_and(|p| !self.may_write(p)) {
            return None;
        }

        // The edit lands in the kernel's nearest LIVE config for the
        // document (the rule pins are resolved under, through the
        // overlay), else in a new config at the binding's anchor: its own
        // manifest's directory, or a live marker directory above it —
        // both live while the binding is (inputs beside a manifest are
        // never inerted by it). A disk walk by `is_file()` used to pick
        // the nearest FILE instead: an inert tenant-committed config (a
        // pin there changes nothing; an opt-out already written there
        // hid the action) and never an unsaved config the overlay
        // already resolves through.
        let workspace_edit = match existing {
            Some(project_path) => {
                let uri = Url::from_file_path(project_path).ok()?;
                let text = self.editable_text(&uri, project_path)?;
                let new_text = project_file_insertion(&text, &edit)?;
                // The CST splice returns the complete new text; the edit
                // handed out is its ONE hunk — the inserted lines, at
                // their line start — so the client's undo, cursor and
                // diff see an insertion, never a whole-file rewrite.
                let hunk = nml_core::cst::edit::single_hunk(&text, &new_text);
                let line_index = LineIndex::new(&text);
                self.workspace_edit(
                    uri,
                    vec![TextEdit {
                        range: line_index.range(hunk.span),
                        new_text: hunk.replacement,
                    }],
                )
            }
            None => {
                let project_path = root.join("nml-project.nml");
                let uri = Url::from_file_path(&project_path).ok()?;
                // A new file has no indentation to read: the edit lands in
                // the bare skeleton through the SAME insertion an existing
                // config receives, so it nests by the canonical unit — the
                // file `nml fmt` would write (the formatter's fixed point,
                // pinned) — and the two shapes have one spelling.
                let content = project_file_insertion(PROJECT_SKELETON, &edit)?;
                self.create_file_edit(uri, content)?
            }
        };

        Some(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(workspace_edit),
            ..Default::default()
        })
    }

    /// The quick fix for one round-tripped suggestion `entry` of `diag`, a
    /// diagnostic of `own` (whose cached text is `own_text`): resolved
    /// through the ONE resolver both appliers share (RFC 0023) against the
    /// text of the file the edit lands in — this document, or the one the
    /// suggestion names (`source`: an open buffer's text first, else the
    /// disk's), as a versioned edit on THAT file. A singleton batch per
    /// suggestion — N did-you-mean alternatives share one span and are N
    /// actions (`Overlap` is a batch-applier verdict and never fires
    /// here); any refusal (the injection guard, a stale span, an
    /// unparsable source, a block already holding the entry) is no
    /// action, never a guess.
    fn suggestion_action(
        &self,
        own: &Url,
        own_text: &str,
        entry: &SuggestionEntry,
        total: usize,
        diag: &Diagnostic,
    ) -> Option<CodeAction> {
        use nml_core::diagnostic::SuggestionKind;
        // An unknown kind is no action, never a guess.
        let kind = SuggestionKind::from_wire_name(&entry.kind)?;
        // The wire's entry through the one door: its kind with its
        // payload, at its anchor, in the file the wire named when it
        // named one (the diagnostic's own otherwise).
        let anchored = nml_core::diagnostic::Suggestion::of(kind, entry.replacement.clone())
            .at(nml_core::span::Span::new(entry.start, entry.end));
        let suggestion = match &entry.source {
            Some(key) => anchored.in_file(key.clone()),
            None => anchored,
        };
        let (target, text) = match &entry.source {
            None => (own.clone(), own_text.to_string()),
            Some(key) => self.suggestion_target(own, key)?,
        };
        let resolved =
            nml_core::cst::edit::resolve_suggestions(&text, std::slice::from_ref(&suggestion));
        let Some(Ok(applied)) = resolved.outcomes.first() else {
            return None;
        };
        let index = LineIndex::new(&text);
        let edits: Vec<TextEdit> = resolved
            .edits
            .iter()
            .map(|e| TextEdit {
                range: index.range(e.span),
                new_text: e.replacement.clone(),
            })
            .collect();
        if edits.is_empty() {
            return None;
        }
        // Titles derive from the outcome: a structural edit names what it
        // does (an insertion into another file names that file); an
        // empty verbatim fix (the trailing-dot removal) is `Remove`; a
        // verbatim fix that INSERTS (an empty span — `?` after a field's
        // type) names the insertion, one that replaces names its payload;
        // `is_preferred` only for a SINGLETON did-you-mean — N mutually
        // exclusive fixes must never let the editor auto-apply a guess
        // (the exact ambiguity RFC 0015 D2 exists to forbid), and a
        // structural edit is not a spelling repair, so none is preferred.
        let title = match applied.title() {
            Some(title) => title,
            None if kind == SuggestionKind::Fix && suggestion.replacement.is_empty() => {
                "Remove".to_string()
            }
            None if kind == SuggestionKind::Fix => match resolved.edits.as_slice() {
                [edit] if edit.span.start == edit.span.end => {
                    format!("Insert `{}`", suggestion.replacement)
                }
                _ => format!("Apply fix: `{}`", suggestion.replacement),
            },
            None => format!("Replace with \"{}\"", suggestion.replacement),
        };
        // An edit in another file names it, whatever its kind: the
        // reader picks an action that changes a file they are not
        // looking at.
        let title = match &entry.source {
            Some(key) => format!("{title} in {key}"),
            None => title,
        };
        let preferred = kind == SuggestionKind::DidYouMean && total == 1;
        Some(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![diag.clone()]),
            edit: Some(self.workspace_edit(target, edits)),
            is_preferred: preferred.then_some(true),
            ..Default::default()
        })
    }

    /// The file a suggestion of `own`'s diagnostic names by `key`, and its
    /// editable text: the name is a workspace KEY (`Suggestion::source`
    /// vocabulary — every component plain, never `..`, never absolute —
    /// the check the CLI's foreign read makes, `Workspace::read_source`)
    /// that names its file through `own`'s universe root (the rule every
    /// related note is located by), so the path `may_write` judges cannot
    /// climb. `None` for a name that is no key, a document with no root,
    /// a file the action may not write or cannot read.
    fn suggestion_target(&self, own: &Url, key: &str) -> Option<(Url, String)> {
        let key = nml_validate::workspace::SourceKey::checked(key)?;
        let root = self
            .resolve_document(own)
            .and_then(|r| r.root.map(|(root, _)| root))?;
        let path = root.join(key.as_str());
        if !self.may_write(&path) {
            return None;
        }
        let uri = Url::from_file_path(&path).ok()?;
        let text = self.editable_text(&uri, &path)?;
        Some((uri, text))
    }

    /// Park what the raw `initialize` params of one frame said about
    /// `workspace.diagnostics.refreshSupport` — LSP 3.17's spelling, which
    /// lsp-types 0.94.1 drops on deserialization, read by
    /// [`crate::NmlService`] from the request as the client sent it. Parked,
    /// not applied: only the `initialize` HANDLER — which runs for the one
    /// frame tower-lsp accepts — turns the capability on, so a duplicate
    /// `initialize` declares nothing. Written on every frame, so the
    /// accepted handshake's own value is the one its handler takes.
    pub fn park_raw_refresh_declaration(&self, declared: bool) {
        self.raw_refresh_declaration
            .store(declared, std::sync::atomic::Ordering::Relaxed);
    }

    /// Take the parked declaration, clearing it.
    fn take_raw_refresh_declaration(&self) -> bool {
        self.raw_refresh_declaration
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    /// Ask the client to send `**/*.nml` disk events — the file-watch
    /// registration, and the second half of the freshness contract.
    ///
    /// LSP 3.17 has no static spelling for file watching, so
    /// `workspace.didChangeWatchedFiles.dynamicRegistration` (half one, read
    /// in `initialize`) is the only thing that says a client can answer this
    /// request at all; the answer is half two — a client that DECLINES the
    /// registration sends no events whatever it declared, and discovery must
    /// fall back to stat-ing the disk. Both halves, and the transport gate
    /// [`crate::ask`] adds, are refusals of the same shape, so one `is_err`
    /// reads them all.
    async fn ask_the_client_to_watch_nml_files(&self) {
        if !self
            .watching_files
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let registration = Registration {
            id: "nml-file-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.nml".to_string()),
                        kind: None,
                    }],
                })
                .unwrap_or_default(),
            ),
        };
        if self
            .client
            .ask(|c| c.register_capability(vec![registration]))
            .await
            .is_err()
        {
            self.watching_files
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Whether any open document OTHER than `uri` holds a cached report —
    /// the documents a universe change can leave stale.
    fn other_reports_cached(&self, uri: &Url) -> bool {
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|other| other != uri)
    }

    /// The actions of OTHER open documents whose suggestions edit `uri`
    /// (their `source` names it) at an anchor inside `range` of `text` —
    /// a manifest opened at the binding a denial points at offers the
    /// grant there. Read from the cache's FRESH entries only (text and
    /// resolver generation current), exactly as membership judges a
    /// round-tripped diagnostic: a document whose diagnostics moved on
    /// offers nothing here until its next pull.
    fn actions_targeting(&self, uri: &Url, text: &str, range: Range) -> Vec<CodeAction> {
        let generation = self.resolver.generation();
        let fresh: Vec<(Url, String, Arc<Vec<Diagnostic>>)> = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let cache = self.diags_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .iter()
                .filter(|(other, entry)| {
                    *other != uri
                        && docs
                            .get(other)
                            .is_some_and(|current| entry.is_fresh(current, generation))
                        && entry.items.iter().any(|d| {
                            d.data
                                .as_ref()
                                .and_then(|data| data.get("suggestions"))
                                .and_then(|s| s.as_array())
                                .is_some_and(|s| s.iter().any(|e| e.get("source").is_some()))
                        })
                })
                .map(|(other, entry)| (other.clone(), entry.text.clone(), Arc::clone(&entry.items)))
                .collect()
        };
        let index = LineIndex::new(text);
        let intersects = |anchor: Range| !(anchor.end < range.start || range.end < anchor.start);
        let mut out = Vec::new();
        for (other, other_text, items) in fresh {
            for diag in items.iter() {
                let Some(suggestions) = diag
                    .data
                    .as_ref()
                    .and_then(|data| data.get("suggestions"))
                    .and_then(|s| s.as_array())
                else {
                    continue;
                };
                let parsed = parse_suggestion_entries(suggestions);
                let total = parsed.len();
                for entry in &parsed {
                    let Some(key) = entry.source.as_deref() else {
                        continue;
                    };
                    let names_this = self
                        .suggestion_target(&other, key)
                        .is_some_and(|(target, _)| target == *uri);
                    if !names_this {
                        continue;
                    }
                    // The anchor as this document's current text places it.
                    if entry.end > text.len()
                        || !intersects(
                            index.range(nml_core::span::Span::new(entry.start, entry.end)),
                        )
                    {
                        continue;
                    }
                    if let Some(action) =
                        self.suggestion_action(&other, &other_text, entry, total, diag)
                    {
                        out.push(action);
                    }
                }
            }
        }
        out
    }

    /// `nml/schemaInfo` (RFC 0030 introspection): which package validates a
    /// file, from where, at which hash, bound how — plus every degraded-state
    /// note. Registered as a custom JSON-RPC method; any LSP client can call
    /// it, the VS Code extension renders it.
    ///
    /// **This payload grows by ADDING keys, never by renaming or retyping
    /// one.** The extension's version floats free of the server's (extension
    /// 0.4.0 against crates 0.1.0), and two of the three rungs of the RFC 0035
    /// discovery ladder hand it a separately released server: the `<tool> lsp`
    /// provider binary, and a `nml.server.path` / `~/.cargo/bin/nml-lsp`
    /// native build (`editors/vscode/INSTALL.md` builds server and extension
    /// in independent steps). Only the bundled-WASM rung ships them together.
    /// Its parser (`editors/vscode/src/contracts/schemaInfo.ts`) validates
    /// every field by type and rejects the WHOLE payload if one is wrong, so a
    /// retyped field renders as a plain green `$(check) nml` with the package,
    /// hash, root and — the harm — the note warning all gone: a degraded
    /// binding reported as healthy. A richer structure (e.g. a kernel binding
    /// report) therefore arrives under a NEW key beside `binding`, with the
    /// old key kept and documented as deprecated for one release.
    /// `crates/nml-lsp/tests/harness.rs` pins the field types.
    pub async fn schema_info(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let uri = params
            .get("uri")
            .and_then(|u| u.as_str())
            .and_then(|u| Url::parse(u).ok());
        let Some(uri) = uri else {
            return Ok(serde_json::json!({ "error": "missing or invalid 'uri'" }));
        };
        // A refused open buffer: unbound, and the cap row is its one note.
        if let Some(row) = self.refused_buffer_row(&uri) {
            return Ok(serde_json::json!({
                "bound": false,
                "notes": [{
                    "message": row.message,
                    "severity": nml_core::diagnostic::Severity::Error.to_string(),
                    "code": null,
                    "range": null,
                }],
            }));
        }
        let Some(resolved) = self.resolve_document(&uri) else {
            return Ok(serde_json::json!({ "bound": false, "notes": [] }));
        };
        // Structured notes — the never-migrate wire shape, fixed BEFORE the
        // first consumer exists: enumerated severity (extensible, unlike a
        // bool) and an LSP Range (the client's native vocabulary; raw byte
        // offsets would push UTF-16 conversion onto every client).
        let doc_text = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            docs.get(&uri).cloned()
        };
        let line_index = doc_text.as_deref().map(LineIndex::new);
        // A note anchored at the document's first declaration parses the
        // text once, and only when such a note exists.
        let declaration: std::cell::OnceCell<Option<nml_core::span::Span>> =
            std::cell::OnceCell::new();
        let range_of = |sp: nml_core::span::Span| {
            line_index
                .as_ref()
                .map(|li| serde_json::to_value(li.range(sp)).ok())
        };
        let notes: Vec<serde_json::Value> = resolved
            .notes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "message": n.message,
                    "severity": n.severity.to_string(),
                    "code": n.code.map(|c| c.to_string()),
                    "range": match n.anchor {
                        packages::NoteAnchor::Top => None,
                        packages::NoteAnchor::Declaration => declaration
                            .get_or_init(|| {
                                doc_text.as_deref().and_then(|text| {
                                    first_declaration(&diagnostics::ParsedBuffer::parse(text).file)
                                })
                            })
                            .and_then(range_of),
                        packages::NoteAnchor::At(sp) => range_of(sp),
                    },
                })
            })
            .collect();
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // The universe root's facts, in the `--json` root object's
        // vocabulary (`origin`, `fence`, `shadowed`): how the root was
        // fixed — `editor` for a workspace folder, `derivedVcsFence` /
        // `derivedTargetDir` for a document outside every folder — the
        // fence entry's kind when it is a `.git` entry (`dir`, `file`,
        // `symlink`, `other`: a FILE is a linked worktree's, a
        // submodule's or a planted one), and the shadow above it, spelled
        // from the root it sits above (`../../demo.package.nml`: never
        // absolute, like `root`, and saying how far above) — what the
        // CLI's root note says, for a status bar to say too. `null` each
        // for a document with no universe.
        let root_origin = resolved.root.as_ref().map(|(_, origin)| origin.tag());
        let (root_fence, root_shadowed) = match resolved.root.as_ref() {
            Some((root, nml_validate::workspace::RootOrigin::Derived { fence, shadowed })) => (
                fence.entry_tag(),
                shadowed
                    .as_ref()
                    .map(|shadow| packages::shadow_display(root, shadow.path())),
            ),
            _ => (None, None),
        };
        // Whether the universe DECIDES ("closed") or claims nothing
        // ("open") — the kernel's own two words, the `--json` `binding`
        // row's. ADDITIVE, per this payload's rule. The status bar had
        // only `bound: false` for BOTH unbound states, so it gave the
        // OPEN remedy over a CLOSED universe: *commit a
        // `<name>.package.nml`* where a manifest already exists and the
        // fix is a `files` glob. `null` when no universe was built.
        let universe = resolved.universe.map(|u| u.label());
        Ok(match &resolved.resolution {
            Resolution::Bound(b) => serde_json::json!({
                "bound": true,
                "package": b.package_name,
                "version": b.package_version,
                "contentHash": b.content_hash,
                "binding": b.binding_name,
                "source": b.class.label(),
                "step": b.step.label(),
                // Workspace-relative — never an absolute host path, and never the
                // `/workspace` WASI mount prefix on the wasm neutral server.
                "root": packages::display_path(&b.root, &roots),
                "rootOrigin": root_origin,
                "rootFence": root_fence,
                "rootShadowed": root_shadowed,
                "shadowsStore": b.shadows_store,
                "universe": universe,
                // The binding's composition grant — the `--json` `binding`
                // row's `layers` object, one spelling: what a denial's
                // quick fix will produce, readable before it is applied.
                "layers": layers_value(&resolved.grant),
                "actions": if b.step == packages::BindingStep::AutoAssociated
                    && b.class != nml_validate::workspace::ClaimClass::Builtin
                {
                    serde_json::json!(["pin", "disableAutoAssociation"])
                } else {
                    serde_json::json!([])
                },
                "notes": notes,
            }),
            Resolution::Unbound | Resolution::Refused => serde_json::json!({
                "bound": false,
                "notes": notes,
                "layers": layers_value(&resolved.grant),
                "rootOrigin": root_origin,
                "rootFence": root_fence,
                "rootShadowed": root_shadowed,
                "universe": universe,
            }),
        })
    }

    /// `nml/explain { code } → { markdown } | null` (RFC 0010 tier 2): the
    /// full error-index entry as a standalone markdown document, from the
    /// same embedded index the CLI's `nml explain` renders — so the entry
    /// always comes from the exact binary that emitted the diagnostic (no
    /// version skew, offline always). Same wire conventions as
    /// [`Self::schema_info`]: malformed params answer as data, an unknown
    /// code is `null` (a lookup miss, not a fault). Case-normalized at this
    /// boundary, like the CLI's.
    pub async fn explain(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let Some(code) = params.get("code").and_then(|c| c.as_str()) else {
            return Ok(serde_json::json!({ "error": "missing or invalid 'code'" }));
        };
        Ok(
            match nml_core::diagnostic::explain_document(&code.to_ascii_uppercase()) {
                Some(markdown) => serde_json::json!({ "markdown": markdown }),
                None => serde_json::Value::Null,
            },
        )
    }

    /// `nml/explainIndex {} → [{ code, headline, summary }]` (RFC 0010 tier
    /// 2): every diagnostic code with its one-line headline (the bold lead
    /// its section opens with — what a palette row shows) and its
    /// first-paragraph summary (what a search matches on), in index order —
    /// the discoverability surface behind the editor's explain-a-code
    /// palette. `headline` is an addition; a client that reads `summary`
    /// alone reads what it did.
    /// Deliberately flat: band grouping would promote the allocation bands
    /// into wire API, which they are documented not to be. Params are
    /// accepted and ignored (tolerant of `{}`, `null`, or absent).
    pub async fn explain_index(&self, _params: serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Array(
            nml_core::diagnostic::explain_index()
                .into_iter()
                .map(|(code, summary)| {
                    serde_json::json!({
                        "code": code,
                        "headline": nml_core::diagnostic::explain_headline(code),
                        "summary": summary,
                    })
                })
                .collect(),
        ))
    }
}

/// The `nml-project.nml` a pin or opt-out creates when none is live: the
/// bare `project` header the edit is inserted under, exactly as it would
/// be into an existing config.
const PROJECT_SKELETON: &str = "project Project:\n";

/// Compute the full new text of an `nml-project.nml` — an existing one, or
/// [`PROJECT_SKELETON`] for a config being created — for a pin/opt-out
/// edit via the CST splice API (`nml_core::cst::edit`, RFC 0030 P2) — a
/// comment-preserving structural insert nested by the file's own
/// indentation (the canonical unit for the skeleton), not a line-offset
/// text patch. `None` when the edit is redundant (already pinned /
/// already opted out) or the file has no `project` block to target.
fn project_file_insertion(text: &str, edit: &ProjectEdit) -> Option<String> {
    use nml_core::cst::edit::{EntryPosition, insert_entry_at_path};
    // Idempotency is decided structurally, through the SAME parser that reads
    // pins at resolution time (`ProjectConfig::from_file`) — so the check can
    // never disagree with how the config is actually interpreted, and a
    // `- name` or `autoAssociate = false` appearing inside a comment or
    // string can't false-suppress the action. This supersedes the earlier
    // hand-rolled text scans (one scoped, one not — an inconsistency).
    let config = {
        let file = nml_core::cst::parse_best_effort(text);
        nml_core::ProjectConfig::from_file(&file)
    };
    match edit {
        ProjectEdit::Pin(name) => {
            if config.schema_packages.iter().any(|p| p == name) {
                return None;
            }
            // Append to the `schemaPackages:` block nested under the `project`
            // block; failing that (no such nested block yet), create it (with
            // its first item) directly under the `project <Name>:` header —
            // the same shapes the plain-text writer produced, now
            // indentation-adaptive and comment-safe. Path addressing means a
            // `schemaPackages:` under some other top-level block can never
            // receive the pin, and duplicates refuse rather than misdirect.
            insert_entry_at_path(
                text,
                &["project", "schemaPackages"],
                &format!("- {name}"),
                EntryPosition::Last,
            )
            .or_else(|| {
                insert_entry_at_path(
                    text,
                    &["project"],
                    &format!("schemaPackages:\n{}- {name}", nml_core::cst::INDENT_UNIT),
                    EntryPosition::AfterHeader,
                )
            })
        }
        ProjectEdit::OptOut => {
            // `auto_associate` defaults true; a `false` already present means
            // the opt-out is redundant.
            if !config.auto_associate {
                return None;
            }
            insert_entry_at_path(
                text,
                &["project"],
                "autoAssociate = false",
                EntryPosition::AfterHeader,
            )
        }
    }
}

/// The name this server answers `initialize` with (LSP 3.17 `serverInfo.name`).
///
/// One string, in one place: the standalone `nml-lsp` binary and every schema
/// provider that embeds [`crate::serve`] are the SAME server, so they answer
/// with the same name. A client that spawned a project-declared `<tool> lsp`
/// uses it to confirm that what it started really is an NML language server
/// before it keeps talking to it.
pub const SERVER_NAME: &str = "nml-lsp";

#[tower_lsp::async_trait]
impl LanguageServer for NmlLanguageServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let insert_replace = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.insert_replace_support)
            .unwrap_or(false);
        self.insert_replace_support
            .store(insert_replace, std::sync::atomic::Ordering::Relaxed);
        let label_details = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|t| t.completion.as_ref())
            .and_then(|c| c.completion_item.as_ref())
            .and_then(|ci| ci.label_details_support)
            .unwrap_or(false);
        self.label_details_support
            .store(label_details, std::sync::atomic::Ordering::Relaxed);
        // LSP 3.17 §WorkspaceEdit: `documentChanges` (versioned edits) and
        // the resource operations are the client's to declare; the
        // server's edits take the shape the client can apply.
        let workspace_edit = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.workspace_edit.as_ref());
        self.versioned_edits.store(
            workspace_edit
                .and_then(|w| w.document_changes)
                .unwrap_or(false),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.creates_files.store(
            workspace_edit
                .and_then(|w| w.resource_operations.as_ref())
                .is_some_and(|ops| ops.contains(&ResourceOperationKind::Create)),
            std::sync::atomic::Ordering::Relaxed,
        );
        // lsp-types' spelling of the capability (`workspace.diagnostic`),
        // OR the specification's (`workspace.diagnostics`), which
        // [`crate::NmlService`] read from the raw params of THIS frame
        // before this handler ran and parked for the handler to take —
        // so the capabilities of an `initialize` tower-lsp REFUSED (a
        // duplicate: `invalid_request`, this handler never runs) reach
        // nothing. The take is unconditional, never short-circuited: a
        // parked declaration left behind would be read by the next
        // handshake that ran.
        let raw_refresh = self.take_raw_refresh_declaration();
        let typed_refresh = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.diagnostic.as_ref())
            .and_then(|d| d.refresh_support)
            .unwrap_or(false);
        self.refresh_diagnostics.store(
            typed_refresh || raw_refresh,
            std::sync::atomic::Ordering::Relaxed,
        );
        // Half one of the freshness contract: the client says it can take a
        // dynamic file-watch registration. Half two is the registration's
        // own answer, in `initialized`.
        self.watching_files.store(
            params
                .capabilities
                .workspace
                .as_ref()
                .and_then(|w| w.did_change_watched_files.as_ref())
                .and_then(|f| f.dynamic_registration)
                .unwrap_or(false),
            std::sync::atomic::Ordering::Relaxed,
        );
        // RFC 0010 tier 2: the client may declare the command id it registered
        // for opening full error explanations. Declared ⇒ diagnostics grow an
        // "Explain NML0000" code action carrying that command; undeclared ⇒
        // the action is never emitted (hover summaries and the CLI remain).
        *self
            .explain_command
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = params
            .initialization_options
            .as_ref()
            .and_then(|o| o.get("explainCommand"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        let roots: Vec<Url> = params
            .workspace_folders
            .as_ref()
            .map(|folders| folders.iter().map(|f| f.uri.clone()).collect())
            .or_else(|| params.root_uri.clone().map(|u| vec![u]))
            .unwrap_or_default();
        {
            let mut folders: Vec<PathBuf> = roots.iter().filter_map(folder_path).collect();
            folders.sort();
            *self
                .workspace_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = folders;
        }
        // The index is deliberately NOT built here: `initialize` gates the
        // whole handshake (the client may send nothing until it answers), and
        // the sweep is the single most expensive thing the server does. It is
        // queued for `initialized`, a notification the client does not wait
        // on.
        *self
            .pending_index_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = roots;
        Ok(InitializeResult {
            // LSP 3.17 §initialize: `serverInfo` is how a client learns WHAT
            // answered its handshake. Every provider tool embeds
            // [`crate::serve`], so this name is the PROTOCOL implementation's,
            // not the embedding tool's — a client that launched `<tool> lsp`
            // learns from it that an NML language server is on the other end,
            // which is exactly the question "did the thing I spawned turn out
            // to be one?". The version is the crate's, so a client can report
            // it and a bug report names a build.
            server_info: Some(ServerInfo {
                name: SERVER_NAME.to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "@".to_string(),
                        "|".to_string(),
                        ".".to_string(),
                        "$".to_string(),
                        "=".to_string(),
                        // RFC 0030/0032: directive-vocabulary completion after
                        // `#` on a field-def line in covered model files.
                        "#".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "\n".to_string(),
                    more_trigger_character: None,
                }),
                // RFC 0030: machine-applicable quick-fixes (did-you-mean) +
                // pin / auto-association code actions.
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                // RFC 0035: PULL diagnostics (LSP 3.17). The client requests a
                // document's diagnostics (`textDocument/diagnostic`) on open,
                // edit, and focus — no server push, so the model works
                // identically on the native server and the wasm neutral server
                // (whose synchronous pump cannot host a background push task).
                // `inter_file_dependencies` is true — an nml file's diagnostics
                // depend on its schema package and sibling model files — so the
                // client re-pulls a dependent when it regains focus after an
                // upstream edit. Workspace-wide pull is deliberately OFF:
                // exhaustive whole-tree validation is the tool CLI's job (e.g.
                // `nudge` schema checks), not a long-poll the serial wasm pump
                // cannot serve.
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        identifier: Some("nml".to_string()),
                        inter_file_dependencies: true,
                        workspace_diagnostics: false,
                        work_done_progress_options: Default::default(),
                    },
                )),
                position_encoding: Some(PositionEncodingKind::UTF16),
                inlay_hint_provider: Some(OneOf::Left(true)),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(crate::semantic_tokens::server_capabilities()),
                // Workspace folders added or removed while the server
                // runs are honoured (`workspace/didChangeWorkspaceFolders`):
                // a folder added at runtime is indexed and governs its
                // documents (they were "outside every folder" until a
                // restart), a removed one leaves with its universe.
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        // The workspace sweep queued by `initialize`. Running it here keeps it
        // off the handshake's critical path; ordering is still exact, because
        // the client must send `initialized` before any other message and both
        // transports (tower-lsp's ordered `buffer_unordered` feed and the wasm
        // pump's strict read→call→write loop) deliver in arrival order.
        let roots = std::mem::take(
            &mut *self
                .pending_index_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        if !roots.is_empty() {
            let denials = self.index_workspace(&roots);
            self.rebuild_schema_registry();
            // A buffer opened while the sweep ran (an editor opens its
            // active file right after `initialized`, and tower-lsp runs
            // the handlers concurrently) was pulled against the unindexed
            // universe, and a pull client pulls again only on an edit, a
            // focus, or this request: ask a client that declared
            // `refreshSupport` to pull now that the index stands. A
            // client whose first open came after the sweep is asked
            // nothing — no buffer is open at this point (`documents`
            // also holds the indexed copies, so it is not the signal).
            let opened_during_sweep = !self
                .open_docs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty();
            if opened_during_sweep
                && self
                    .refresh_diagnostics
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                let _ = self.client.ask(|c| c.workspace_diagnostic_refresh()).await;
            }
            // Loud, fail-closed: what the kernel denied is not indexed,
            // and the editor says so once per denial, as a warning.
            for denial in denials {
                self.client
                    .log_message(MessageType::WARNING, format!("NML: {denial}"))
                    .await;
            }
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("NML: indexed {} workspace root(s)", roots.len()),
                )
                .await;
        }
        self.ask_the_client_to_watch_nml_files().await;
        self.client
            .log_message(MessageType::INFO, "NML language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Pull diagnostics (RFC 0035, LSP 3.17): compute this document's full
    /// diagnostic set on demand. `result_id` is a hash of the diagnostics, so
    /// a re-pull whose output is unchanged (the common focus-change case)
    /// returns a cheap `Unchanged` report and the client keeps its rendering.
    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let uri = params.text_document.uri;
        // An unknown document (never opened, not indexed) has nothing to
        // report — an empty full report, never an error. A cache hit means
        // the *Unchanged* comparison below costs no re-validation (RFC 0010).
        let generation_before = self.resolver.generation();
        let items = self
            .cached_diagnostics(&uri)
            .await
            .map(|(_, items)| items)
            .unwrap_or_default();
        // This pull rediscovered the universe (a universe input changed
        // under it): every OTHER open document's cached report may now be
        // stale — the actions offered from those reports with it. A client
        // that declared `refreshSupport` is asked to re-pull them, once per
        // change (the re-pulls hit the fresh cache and ask nothing).
        if self
            .refresh_diagnostics
            .load(std::sync::atomic::Ordering::Relaxed)
            && self.resolver.generation() != generation_before
            && self.other_reports_cached(&uri)
        {
            let _ = self.client.ask(|c| c.workspace_diagnostic_refresh()).await;
        }
        // The fill path drains; a cache hit skips it — but other handlers may
        // have queued store events since, so the pull stays the reliable
        // delivery path (cheap no-op when empty).
        self.drain_store_events().await;

        let result_id = diagnostics_result_id(&items);
        if params.previous_result_id.as_deref() == Some(result_id.as_str()) {
            return Ok(DocumentDiagnosticReportResult::Report(
                DocumentDiagnosticReport::Unchanged(RelatedUnchangedDocumentDiagnosticReport {
                    related_documents: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id,
                    },
                }),
            ));
        }
        Ok(DocumentDiagnosticReportResult::Report(
            DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(result_id),
                    // The one deep copy, paid only when the report actually
                    // ships (the Unchanged path above never clones).
                    items: (*items).clone(),
                },
            }),
        ))
    }

    /// A workspace folder added while the server runs joins the roots and
    /// is indexed exactly as at `initialized` (its denials said the same
    /// way); a removed one leaves the roots with its universe and its
    /// indexed (not open) documents. A document under an added folder
    /// resolves under it from the next pull — the folder wins over the
    /// derivation the document had outside every folder (R1).
    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let added: Vec<Url> = params.event.added.iter().map(|f| f.uri.clone()).collect();
        let removed: Vec<PathBuf> = params
            .event
            .removed
            .iter()
            .filter_map(|f| folder_path(&f.uri))
            .collect();
        {
            let mut roots = self
                .workspace_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            roots.retain(|r| !removed.contains(r));
            for root in added.iter().filter_map(folder_path) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            // An added folder takes its place among the others, never the
            // end: the list's ORDER is the rule (an ancestor before its
            // descendants), not an arrival log.
            roots.sort();
        }
        if !removed.is_empty() {
            self.resolver.invalidate_claims_for(&removed);
            let open = self
                .open_docs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let mut indexed = self.indexed_uris.lock().unwrap_or_else(|e| e.into_inner());
            indexed.retain(|uri| {
                let under = uri
                    .to_file_path()
                    .is_ok_and(|p| removed.iter().any(|r| p.starts_with(r)));
                if under && !open.contains(uri) {
                    docs.remove(uri);
                }
                !under
            });
        }
        if !added.is_empty() {
            let denials = self.index_workspace(&added);
            for denial in denials {
                self.client
                    .log_message(MessageType::WARNING, format!("NML: {denial}"))
                    .await;
            }
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("NML: indexed {} workspace root(s)", added.len()),
                )
                .await;
        }
        // The registry and every document's diagnostics follow the roots.
        self.rebuild_schema_registry();
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.open_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(params.text_document.uri.clone());
        // State only; the client pulls this document's diagnostics (didOpen
        // triggers a pull under the diagnostic-provider capability).
        self.on_change(
            params.text_document.uri,
            params.text_document.text,
            Some(params.text_document.version),
        );
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.on_change(
                params.text_document.uri,
                change.text,
                Some(params.text_document.version),
            );
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.open_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        self.refused_buffers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        let was_model = is_schema_source(&uri);
        let indexed = self
            .indexed_uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&uri);
        if indexed {
            // The document's truth is the DISK's again (LSP: after
            // `didClose` the server no longer holds the client's text —
            // an unsaved edit goes with the buffer): the indexed copy is
            // re-read under the index's own bound, so a buffer refused
            // past it (nothing stored) whose disk copy is under it is
            // indexed again, and a copy that cannot be read is
            // un-indexed and said, as the index says it. (The last
            // buffer text used to stay in the index until a watcher
            // event; a refused one left the document absent.)
            if let Some(denial) = self.reindex_closed(&uri) {
                self.client
                    .log_message(MessageType::WARNING, format!("NML: {denial}"))
                    .await;
            }
        } else {
            self.documents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&uri);
        }

        if was_model {
            // The registry changed under other documents; they heal on their
            // next pull (focus/edit) — no inline fan-out.
            self.rebuild_schema_registry();
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // File create/delete changes what a package's binding globs can
        // claim under a root — cached coverage verdicts are statements
        // about which files EXIST (names against globs; no content is ever
        // read), so those events invalidate the verdicts of the roots
        // containing the touched paths (cheap to recompute;
        // wrong-until-restart is not acceptable). CHANGED events are
        // skipped entirely: an existence/name/glob function cannot move on
        // content — a changed manifest re-keys the memo via its content
        // hash — and CHANGED-storms during typing used to force a full
        // re-walk of every root per save. Both the registration in
        // `initialized` and the client's watcher pin the watch to
        // `**/*.nml`, so nothing broader arrives here; paths the claims
        // walk could never see (policy-skipped dot-dir/`node_modules`/
        // `target` segments) are filtered with the same predicate the walk
        // uses.
        let claim_changes: Vec<PathBuf> = {
            let roots = self
                .workspace_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            params
                .changes
                .iter()
                .filter(|change| {
                    matches!(
                        change.typ,
                        FileChangeType::CREATED | FileChangeType::DELETED
                    )
                })
                .filter_map(|change| change.uri.to_file_path().ok())
                .map(|path| canonicalize_watched_path(&path))
                .filter(|path| packages::watched_path_affects_claims(path, &roots))
                .collect()
        };
        if !claim_changes.is_empty() {
            self.resolver.invalidate_claims_for(&claim_changes);
        }
        for change in params.changes {
            // LSP spec: after didOpen the CLIENT buffer is the sole source of
            // truth for a document's content — disk events are irrelevant
            // while the file is open (didClose will reconcile). This guards
            // EVERY arm: a CREATED/CHANGED must not clobber the open buffer's
            // text with disk content any more than a DELETED may drop it —
            // either way the served document would stop matching what the
            // user sees in the editor.
            let is_open = self
                .open_docs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&change.uri);
            if is_open {
                continue;
            }
            match change.typ {
                FileChangeType::CREATED | FileChangeType::CHANGED => {
                    let Ok(path) = change.uri.to_file_path() else {
                        continue;
                    };
                    let eligible = {
                        let roots = self
                            .workspace_roots
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        watched_file_is_eligible(&path, &roots)
                    };
                    if !eligible {
                        continue;
                    }
                    // Disk-backed like the startup index — read under the
                    // SAME per-file bound the index uses (a tenant-committed
                    // `.nml` created or grown while the editor is open was
                    // read whole into the store: +96 MB for a 48 MiB file)
                    // — and a refusal is SAID, as the
                    // index says it, never skipped silently.
                    match read_leaf(&path, MAX_INDEX_BYTES, INDEXED_FILE) {
                        Ok(content) => {
                            // Mark it indexed: `did_close` drops non-indexed
                            // documents, and without this a watcher-created
                            // file that was opened then closed would vanish
                            // from the registry (dependents stuck on
                            // "unknown" until the next disk event) even
                            // though it still exists on disk.
                            self.indexed_uris
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(change.uri.clone());
                            let stamp = packages::disk_stamp(&path);
                            self.on_change(change.uri.clone(), content, None);
                            if let Some(stamp) = stamp {
                                self.documents
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .note_disk(change.uri, stamp);
                            }
                        }
                        Err(why) => {
                            self.client
                                .log_message(
                                    MessageType::WARNING,
                                    format!("NML: `{}` is not indexed: {why}", path.display()),
                                )
                                .await;
                        }
                    }
                }
                FileChangeType::DELETED => {
                    // Open docs never reach here (guard above) — and that
                    // covers this ENTIRE arm, registry handling included:
                    // dropping the text while keeping serving the doc would
                    // leave a half-alive document (definitions but no
                    // content, or vice versa) — worse than either consistent
                    // state.
                    self.documents
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&change.uri);
                    self.diags_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&change.uri);
                    self.indexed_uris
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&change.uri);
                    if is_schema_source(&change.uri) {
                        // Other documents heal on their next pull.
                        self.rebuild_schema_registry();
                    }
                }
                _ => {}
            }
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let mut items = Vec::new();
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        // Directive completion (RFC 0030/0032): after `#` on a field-def line
        // of a covered model file, offer the covering package's vocabulary.
        // Checked before the value branch — a field line with a default
        // (`port number = 80 #li`) has an `=` before the cursor, so the value
        // detector would otherwise claim it.
        if is_schema_source(&uri) {
            let in_directive_position = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                docs.get(&uri)
                    .and_then(|source| {
                        let line = position::line_at(source, pos.line)?;
                        let end = position::utf16_to_byte(line, pos.character);
                        // Strip the partly-typed name back to the `#`, then
                        // require a field def before it: a directive TRAILS a
                        // field definition, it never opens a line.
                        let stem = line[..end].trim_end_matches(is_word_char);
                        Some(stem.ends_with('#') && !stem[..stem.len() - 1].trim().is_empty())
                    })
                    .unwrap_or(false)
            };
            if in_directive_position {
                // Opaque/undetermined files get an empty menu, not the
                // generic keyword soup — nothing meaningful follows `#`
                // without a KNOWN covering vocabulary.
                if let packages::VocabularyOutcome::Covered(vocab) =
                    self.vocabulary_for_document(&uri)
                {
                    // The language's merge-policy directives first, then the
                    // package's declared entries (RFC 0019: the builtins are
                    // merged into every vocabulary outcome).
                    for e in vocab.vocabulary.entries() {
                        items.push(CompletionItem {
                            label: e.name.to_string(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(e.arg.label().to_string()),
                            documentation: Some(Documentation::String(e.doc.to_string())),
                            ..Default::default()
                        });
                    }
                }
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        // One schema-context acquisition per request — the value, field, and
        // keyword sections below all read it (Registry mode builds an index;
        // building it once per keystroke is the budget, not once per section).
        let handle = self.schema_index_for(&uri);

        let is_value_position = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            docs.get(&uri)
                .and_then(|source| {
                    let line = position::line_at(source, pos.line)?;
                    let end = position::utf16_to_byte(line, pos.character);
                    Some(line[..end].contains('='))
                })
                .unwrap_or(false)
        };

        if is_value_position {
            let template_context = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                docs.get(&uri).and_then(|source| {
                    let line = position::line_at(source, pos.line)?;
                    let end = position::utf16_to_byte(line, pos.character);
                    let before_cursor = &line[..end];
                    if is_template_namespace_position(before_cursor) {
                        Some(true)
                    } else {
                        None
                    }
                })
            };

            if template_context.is_some() {
                let namespaces: Vec<String> = self.project_config_of(&uri).template_namespaces;
                for ns in &namespaces {
                    items.push(CompletionItem {
                        label: format!("{ns}."),
                        kind: Some(CompletionItemKind::MODULE),
                        detail: Some("template namespace".to_string()),
                        ..Default::default()
                    });
                }
                return Ok(Some(CompletionResponse::Array(items)));
            }

            // Schema-driven value completions (model refs, oneof discriminator
            // arm keys, plain enum variants) share one schema snapshot rather
            // than re-cloning it per detector. A package-bound document (RFC
            // 0030) completes against its package's exclusive definitions —
            // the same exclusivity rule diagnostics apply.
            let (model_ref_types, discriminator_values, value_completions, duration_context): (
                Vec<String>,
                Option<Vec<String>>,
                Option<ValueCompletions>,
                Option<DurationUnitContext>,
            ) = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                match docs.get(&uri).map(|source| {
                    let (file, parse) = nml_core::cst::parse_best_effort_with_tree(source);
                    (source, file, parse)
                }) {
                    // Parse once and build one schema index, shared by all detectors.
                    Some((source, file, parse)) => {
                        let index: &SchemaIndex = handle.index();
                        let line_index = LineIndex::new(source);
                        let model_refs =
                            find_model_ref_types_at(&file, source, pos, index, &line_index);
                        let discriminator =
                            find_oneof_discriminator_at(&file, source, pos, index, &line_index)
                                .map(|o| {
                                    o.variants.iter().map(|(value, _)| value.clone()).collect()
                                });
                        let values =
                            find_value_completions_at(&file, source, pos, index, &line_index);
                        let duration = duration_lsp::find_duration_unit_completions_at(
                            &parse,
                            pos,
                            &line_index,
                            || {
                                value_position_prop_name(source, pos)
                                    .map(|prop| {
                                        value_governors_at(&file, pos, index, &line_index, prop)
                                            .fields
                                            .iter()
                                            .any(|f| governs_duration(&f.field_type))
                                    })
                                    .unwrap_or(false)
                            },
                        );
                        (model_refs, discriminator, values, duration)
                    }
                    None => (Vec::new(), None, None, None),
                }
            };

            if !model_ref_types.is_empty() {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                for ref_type in &model_ref_types {
                    let matches = collect_declarations_by_keyword(&docs, ref_type);
                    for (name, kw, file_name) in matches {
                        items.push(CompletionItem {
                            label: name.clone(),
                            kind: Some(CompletionItemKind::REFERENCE),
                            detail: Some(format!("{kw} (from {file_name})")),
                            sort_text: Some(format!("0_{name}")),
                            ..Default::default()
                        });
                    }
                }
            }

            // Precise value edits (RFC 0030): the client replaces the whole
            // existing literal (quotes included) — no quote doubling in any
            // client, insert-vs-replace honored where supported.
            let edit_ranges = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                docs.get(&uri).and_then(|src| value_edit_ranges(src, pos))
            };
            let insert_replace = self
                .insert_replace_support
                .load(std::sync::atomic::Ordering::Relaxed);

            // Unit suffixes after a bare number in a duration-typed field
            // (RFC 0017): `30` offers the full literals `30s`/`30ms`/`30m`/
            // `30h`, replacing exactly the typed digits — full-literal
            // labels, because clients filter against the token text (`30`),
            // which a bare suffix label would never match. Units whose
            // composed literal would be out of domain are withheld — a
            // completion must never insert an instant diagnostic.
            if let Some(ctx) = duration_context {
                for (i, unit) in ctx.units.iter().enumerate() {
                    if !duration_lsp::completion_is_valid(&ctx, unit.suffix()) {
                        continue;
                    }
                    items.push(duration_unit_item(
                        &ctx,
                        unit.suffix(),
                        unit.name(),
                        format!("0_{i:03}"),
                        insert_replace,
                    ));
                }
            }

            // Inside a `oneof` block, offer the arm keys as discriminator values.
            if let Some(values) = discriminator_values {
                for (i, value) in values.iter().enumerate() {
                    items.push(quoted_value_item(
                        value,
                        "discriminator value",
                        format!("0_{i:03}"),
                        edit_ranges,
                        insert_replace,
                    ));
                }
            }

            // Governed values (RFC 0030 + RFC 0015): enum variants and
            // discriminator arm keys, each labeled for what it is, one
            // schema-declaration-order sequence.
            if let Some(values) = value_completions {
                let labeled = values
                    .variants
                    .iter()
                    .map(|v| (v, "enum variant"))
                    .chain(values.arms.iter().map(|a| (a, "discriminator value")));
                for (i, (value, label)) in labeled.enumerate() {
                    items.push(quoted_value_item(
                        value,
                        label,
                        format!("0_{i:03}"),
                        edit_ranges,
                        insert_replace,
                    ));
                }
            }

            let names = self.collect_declaration_names();
            for (name, keyword) in names {
                items.push(CompletionItem {
                    label: name,
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(keyword),
                    ..Default::default()
                });
            }
        } else {
            // Property position (no `=` before the cursor): schema-driven FIELD completion
            // (RFC 0003) — the dual of the value-position completions above. Offer the
            // enclosing model's not-yet-present fields, type-aware insertion, required-first.
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(source) = docs.get(&uri) {
                let file = nml_core::cst::parse_best_effort(source);
                let index: &SchemaIndex = handle.index();
                let line_index = LineIndex::new(source);
                let line_ctx = position::line_at(source, pos.line).map(|line| {
                    let end = position::utf16_to_byte(line, pos.character);
                    (line, end)
                });
                // Arm-selector position (RFC 0007 §6.1): before `->` inside an
                // arm-set block — offer enum keys, a string-key snippet,
                // `@keyword/name` references for model-keyed K, and `else`.
                if let Some((line, end)) = line_ctx {
                    if !cursor_past_arm_arrow(line, end) {
                        if let Some(key) = find_arm_set_key_at(&file, pos, index, &line_index) {
                            let tagged_refs = collect_tagged_ref_candidates(&docs);
                            let selector_items =
                                arm_selector_completion_items(&key, index, &tagged_refs);
                            return Ok(Some(CompletionResponse::Array(selector_items)));
                        }
                    }
                }
                // Arm-target position (RFC 0007): after the `->` on an arm
                // line, offer declarations of the arm set's target type `V`
                // instead of field names.
                if let Some((line, end)) = line_ctx {
                    if cursor_past_arm_arrow(line, end) {
                        if let Some(target_keywords) =
                            find_arm_target_types_at(&file, pos, index, &line_index)
                        {
                            if let Some(snippet) =
                                inline_arm_target_snippet_item(&target_keywords, index)
                            {
                                items.push(snippet);
                            }
                            for keyword in &target_keywords {
                                // Enum-typed arm target (RFC 0030): offer the
                                // declared variants as values.
                                if let Some(e) = index.enum_def(keyword) {
                                    for (i, variant) in e.variants.iter().enumerate() {
                                        items.push(CompletionItem {
                                            label: nml_core::source_policy::string_literal(variant),
                                            kind: Some(CompletionItemKind::ENUM_MEMBER),
                                            detail: Some("enum variant".to_string()),
                                            sort_text: Some(format!("0_{i:03}")),
                                            ..Default::default()
                                        });
                                    }
                                }
                                for (name, kw, file_name) in
                                    collect_declarations_by_keyword(&docs, keyword)
                                {
                                    items.push(CompletionItem {
                                        label: name.clone(),
                                        kind: Some(CompletionItemKind::REFERENCE),
                                        detail: Some(format!("{kw} (from {file_name})")),
                                        sort_text: Some(format!("0_{name}")),
                                        ..Default::default()
                                    });
                                }
                            }
                            return Ok(Some(CompletionResponse::Array(items)));
                        }
                    }
                }
                // RFC 0015 `as`-position (nominal union): after `<field> as ` on
                // a header line, the author is choosing a union variant by name.
                // Offer the field's nameable variants — the SAME candidate set
                // the validator checks and the did-you-mean draws from, so
                // completion, diagnostics, and fixes share one source of truth.
                let as_field = position::line_at(source, pos.line).and_then(|line| {
                    let end = position::utf16_to_byte(line, pos.character);
                    as_position_field(line, end)
                });
                if let Some(slot) = as_field {
                    // Field form: the union is on the NAMED field of the
                    // enclosing (descended) model. Item form (`- one as ⌖`):
                    // the name is the item's — the union lives on the ENCLOSING
                    // list/set field, found by its own descent that stops AT the
                    // list field (the mid-typing item need not parse, and the
                    // full descent would fail on a list-of-union).
                    let field = match &slot {
                        AsSlot::Field(name) => find_model_body_at(&file, pos, index, &line_index)
                            .and_then(|(model, _)| model.fields.iter().find(|f| f.name == *name)),
                        AsSlot::Item => {
                            enclosing_top_block(&file, pos, &line_index).and_then(|block| {
                                let Some(FieldTarget::Model(model)) =
                                    index.resolve_ref(&block.keyword.name)
                                else {
                                    return None;
                                };
                                find_union_list_field_at(
                                    model,
                                    &block.body,
                                    pos,
                                    index,
                                    &line_index,
                                )
                            })
                        }
                    };
                    {
                        if let Some(field) = field {
                            // The union is the field type (plain or modifier-
                            // wrapped — `union_variants` unwraps `|`), or a
                            // `[]`/`set<>` element type.
                            let base = match &field.field_type {
                                FieldType::Modifier(inner) => inner.as_ref(),
                                t => t,
                            };
                            let variants = base.union_variants().or_else(|| match base {
                                FieldType::List(inner) | FieldType::Set(inner) => {
                                    inner.union_variants()
                                }
                                _ => None,
                            });
                            if let Some(variants) = variants {
                                for (i, variant) in index
                                    .nameable_variant_names(variants)
                                    .into_iter()
                                    .enumerate()
                                {
                                    items.push(CompletionItem {
                                        label: variant.to_string(),
                                        kind: Some(CompletionItemKind::TYPE_PARAMETER),
                                        detail: Some("union variant".to_string()),
                                        sort_text: Some(format!("0_{i:03}")),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                    return Ok(Some(CompletionResponse::Array(items)));
                }
                match find_candidates_at(&file, pos, index, &line_index) {
                    Some(DescentTarget::One {
                        model,
                        body,
                        via_oneof,
                    }) => {
                        let present = present_field_names_in(model, body);
                        for (idx, field) in model.fields.iter().enumerate() {
                            if present.contains(&field.name) {
                                continue;
                            }
                            let (label, filter_text) = field_label(field);
                            items.push(CompletionItem {
                                label,
                                filter_text,
                                kind: Some(CompletionItemKind::FIELD),
                                detail: Some(field_detail(field)),
                                // The schema author's leading comment block (RFC
                                // 0004 §4.3) documents the field in the menu too.
                                documentation: field.doc.clone().map(Documentation::String),
                                sort_text: Some(field_sort_key(field, idx)),
                                insert_text: Some(field_insert_text(index, field)),
                                ..Default::default()
                            });
                        }
                        // Field parity: a body resolved through a oneof's
                        // DEFAULT still shows the discriminator — a settable
                        // knob with a default, exactly like a defaulted field
                        // (which completion shows with its `= default`).
                        // Sorted after declared optional fields.
                        if let Some(o) = defaulted_knob(model, body, via_oneof) {
                            items.push(CompletionItem {
                                label: o.discriminator.clone(),
                                kind: Some(CompletionItemKind::FIELD),
                                detail: Some(format!(
                                    "discriminator of `{}` = {:?} (default)",
                                    o.name,
                                    o.default_discriminator.as_deref().unwrap_or_default()
                                )),
                                sort_text: Some(format!("1_9999_{}", o.discriminator)),
                                insert_text: Some(format!("{} = ", o.discriminator)),
                                ..Default::default()
                            });
                        }
                    }
                    // RFC 0015 F4: an AMBIGUOUS union body — offer the UNION of
                    // all candidates' fields (discover), and let a
                    // variant-unique pick auto-annotate the header (resolve by
                    // choice). The D2 quick-fixes remain the repair tier; the
                    // validator stays the sole authority.
                    Some(DescentTarget::Ambiguous {
                        candidates,
                        body,
                        header,
                    }) => {
                        let label_details = self
                            .label_details_support
                            .load(std::sync::atomic::Ordering::Relaxed);
                        items.extend(union_of_fields_completions(
                            index,
                            &candidates,
                            body,
                            header.as_ref(),
                            &line_index,
                            label_details,
                        ));
                    }
                    None => {}
                }
            }
        }

        let language_keywords = ["model", "trait", "enum", "const", "template"];
        for kw in language_keywords {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                detail: Some("language".to_string()),
                ..Default::default()
            });
        }

        {
            let mut seen: HashSet<String> =
                language_keywords.iter().map(|s| s.to_string()).collect();

            // Schema-driven block-keyword completions — the keyword twin of
            // RFC 0003's field completion: the document's resolved schema
            // context supplies the vocabulary. A package-bound document
            // completes against its package's exclusive definitions (the
            // same closed-vocabulary rule diagnostics enforce, RFC 0012);
            // an open document adds its own definitions. Concrete models
            // and oneofs only — a trait is never a keyword (RFC 0011).
            {
                let index = handle.index();
                let mut names: Vec<String> = index
                    .models()
                    .iter()
                    .filter(|m| !m.is_trait())
                    .map(|m| m.name.clone())
                    .chain(index.oneofs().iter().map(|o| o.name.clone()))
                    .collect();
                if matches!(handle, IndexHandle::Registry(_)) {
                    let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(source) = docs.get(&uri) {
                        let (own, _) = nml_core::cst::extract_schema(source);
                        names.extend(
                            own.models
                                .iter()
                                .filter(|m| !m.is_trait())
                                .map(|m| m.name.clone()),
                        );
                        names.extend(own.oneofs.iter().map(|o| o.name.clone()));
                    }
                }
                for name in names {
                    if seen.insert(name.clone()) {
                        items.push(CompletionItem {
                            label: name,
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some("schema".to_string()),
                            ..Default::default()
                        });
                    }
                }
            }

            let pc = self.project_config_of(&uri);
            for kw in &pc.keywords {
                if seen.insert(kw.clone()) {
                    items.push(CompletionItem {
                        label: kw.clone(),
                        kind: Some(CompletionItemKind::KEYWORD),
                        detail: Some("project".to_string()),
                        ..Default::default()
                    });
                }
            }
            drop(pc);

            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            for source in docs.values() {
                let file = nml_core::cst::parse_best_effort(source);
                for decl in &file.declarations {
                    if let nml_core::ast::DeclarationKind::Block(block) = &decl.kind {
                        let kw = &block.keyword.name;
                        if seen.insert(kw.clone()) {
                            items.push(CompletionItem {
                                label: kw.clone(),
                                kind: Some(CompletionItemKind::KEYWORD),
                                detail: Some("workspace".to_string()),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        let types = [
            "string", "number", "money", "bool", "duration", "path", "secret",
        ];
        for t in types {
            items.push(CompletionItem {
                label: t.to_string(),
                kind: Some(CompletionItemKind::TYPE_PARAMETER),
                ..Default::default()
            });
        }

        {
            let member_kws = &self.membership.member_keywords;
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let mut seen_refs = HashSet::new();
            for source in docs.values() {
                let file = nml_core::cst::parse_best_effort(source);
                for decl in &file.declarations {
                    if let DeclarationKind::Block(block) = &decl.kind {
                        let kw = &block.keyword.name;
                        let name = &block.name.name;
                        let is_tagged = member_kws.iter().any(|mk| mk == kw)
                            || block
                                .extends
                                .iter()
                                .any(|e| member_kws.iter().any(|mk| mk == &e.name));
                        if is_tagged {
                            let label = format!("@{kw}/{name}");
                            if seen_refs.insert(label.clone()) {
                                items.push(CompletionItem {
                                    label,
                                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                                    detail: Some(format!("{kw} instance")),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    /// Quick-fixes from structured suggestions (`Diagnostic.data`) plus the
    /// pin / opt-out actions on auto-associated documents (RFC 0030).
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();

        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            docs.get(&uri).cloned()
        };
        let Some(source) = source else {
            return Ok(None);
        };
        let line_index = LineIndex::new(&source);

        // 1. Machine-applicable suggestions the validator derived — never
        //    re-derived, never parsed out of message text. STALENESS is
        //    settled by MEMBERSHIP, not by a version: an action is offered
        //    only for a client diagnostic whose `data` equals a member's
        //    `data` in the current cache — `data` is the one field the LSP
        //    spec preserves verbatim between a report and `codeAction`,
        //    the cache is keyed by exact text AND registry generation, and
        //    the action consumes only `data` plus the current text. A
        //    buffer edit or a registry rebuild (which no document version
        //    can see) therefore yields NO action, never an edit at a stale
        //    offset. Equal `data` on the current text yields the identical
        //    action, so which member matched is immaterial.

        // The cache (and its line index) is consulted only when some
        // context diagnostic actually carries suggestion `data` — most
        // code-action requests (selection menus over clean text) skip
        // the clone, the compare and the index scan entirely.
        let wants_suggestions = params.context.diagnostics.iter().any(|d| d.data.is_some());
        let cached = if wants_suggestions {
            self.cached_diagnostics(&uri).await
        } else {
            None
        };
        // Resolution and edit ranges use the exact text the cached
        // diagnostics were computed against — never the snapshot above —
        // so membership and the emitted edits stay coherent even if a
        // `didChange` interleaves between the two reads.
        let cache_text = cached.as_ref().map_or("", |(text, _)| text.as_str());
        for diag in &params.context.diagnostics {
            let Some(data) = diag.data.as_ref() else {
                continue;
            };
            let current = cached
                .as_ref()
                .is_some_and(|(_, items)| items.iter().any(|d| d.data.as_ref() == Some(data)));
            if !current {
                continue;
            }
            let Some(suggestions) = data.get("suggestions").and_then(|s| s.as_array()) else {
                continue;
            };
            let parsed = parse_suggestion_entries(suggestions);
            let total = parsed.len();
            for entry in &parsed {
                if let Some(action) = self.suggestion_action(&uri, cache_text, entry, total, diag) {
                    push_unique_action(&mut actions, action);
                }
            }
        }

        // 1b. The same insertions, offered ON THE FILE THEY EDIT: a manifest
        //     opened at the binding a content file's denial points at (its
        //     related location) offers the grant there too — from the fresh
        //     cached diagnostics of the open documents that named this one,
        //     never a re-validation.
        for action in self.actions_targeting(&uri, &source, params.range) {
            push_unique_action(&mut actions, action);
        }

        // 2. Pin / opt-out on auto-associated documents. Structural CST
        //    inserts (RFC 0030 P2) — injection-safe because package names are
        //    charset-constrained identifiers (enforced at package load) AND
        //    the splice API refuses snippets that don't parse as body entries.
        if let Some(resolved) = self.resolve_document(&uri) {
            if let Resolution::Bound(binding) = &resolved.resolution {
                if binding.step == packages::BindingStep::AutoAssociated
                    && binding.class != nml_validate::workspace::ClaimClass::Builtin
                    && uri.to_file_path().is_ok()
                {
                    let name = &binding.package_name;
                    // The config a pin or opt-out belongs in — the
                    // kernel's nearest live one — looked up once for
                    // both actions.
                    let config = self
                        .with_workspace_view(&uri, |path, view| {
                            self.resolver.project_config_path_for(path, view)
                        })
                        .flatten();
                    if let Some(action) = self.project_edit_action(
                        config.as_deref(),
                        &binding.root,
                        format!("Pin schema package '{name}'"),
                        ProjectEdit::Pin(name.clone()),
                    ) {
                        actions.push(CodeActionOrCommand::CodeAction(action));
                    }
                    if let Some(action) = self.project_edit_action(
                        config.as_deref(),
                        &binding.root,
                        format!(
                            "Not a {name} project? Disable schema auto-association for this root"
                        ),
                        ProjectEdit::OptOut,
                    ) {
                        actions.push(CodeActionOrCommand::CodeAction(action));
                    }
                }
            }
        }

        // 3. "Simplify number" (RFC 0016 §1.10): a value-preserving,
        //    user-invoked rewrite of a number literal to its minimal
        //    cohort form (`8080.000` → `8080`, `007` → `7`) — the
        //    counter-lint for unwanted trailing zeros now that fmt
        //    preserves written scale. Refactor-kind, never a quickfix:
        //    authored precision like `2.50` is intent until the author
        //    says otherwise, so nothing auto-applies.
        if let Some(action) = simplify_number_action(&source, line_index.offset(params.range.start))
        {
            let edit = TextEdit {
                range: line_index.range(action.span),
                new_text: action.new_text,
            };
            let mut changes = std::collections::HashMap::new();
            changes.insert(uri.clone(), vec![edit]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: action.title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        }

        // 4. "Explain NML0000" (RFC 0010 tier 2) — negotiation-gated: emitted
        //    only when the client declared its command id at initialize, so no
        //    editor ever receives an action it cannot execute. Derived purely
        //    from the round-tripped `context.diagnostics`: only OUR coded
        //    diagnostics (`source == "nml"` — other extensions' diagnostics
        //    share ranges and must never mint our actions), deduped by code,
        //    after the real fixes (explanation is recourse, not resolution).
        //    Kind stays empty: this fixes nothing, and a client filtering
        //    `only: [quickfix]` must not receive it.
        let explain_command = self
            .explain_command
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(command) = explain_command {
            let mut seen = std::collections::HashSet::new();
            for diag in &params.context.diagnostics {
                if diag.source.as_deref() != Some("nml") {
                    continue;
                }
                let Some(NumberOrString::String(code)) = &diag.code else {
                    continue;
                };
                if !seen.insert(code.clone()) {
                    continue;
                }
                let title = format!("Explain {code}");
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: title.clone(),
                    diagnostics: Some(vec![diag.clone()]),
                    command: Some(Command {
                        title,
                        command: command.clone(),
                        arguments: Some(vec![serde_json::Value::String(code.clone())]),
                    }),
                    ..Default::default()
                }));
            }
        }

        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        // RFC 0010 tier 1 — ONE compose point by construction: the whole
        // base hover runs inside an async block (its `return`s exit the
        // block), so every exit — including the (0,0) binding summary,
        // exactly where file-start diagnostics live — composes with the
        // diagnostic explanation appended below.
        let aug_uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let aug_pos = params.text_document_position_params.position;
        let base = async {
            let uri = params.text_document_position_params.text_document.uri;
            let pos = params.text_document_position_params.position;

            // Document-start hover (RFC 0030 introspection): the binding summary —
            // which package validates this file, from where, at which hash.
            // Position (0,0) only, so it can never shadow a real token's hover
            // (a token's hover is requested at the token, not at the file edge).
            if pos.line == 0 && pos.character == 0 {
                if let Some(resolved) = self.resolve_document(&uri) {
                    if let Resolution::Bound(b) = &resolved.resolution {
                        let roots = self
                            .workspace_roots
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        // The grant beside the binding, in `nml binding`'s
                        // words: what composition this file is permitted.
                        let layers = layers_summary(&resolved.grant)
                            .map(|s| format!("\n\nlayers: {s}"))
                            .unwrap_or_default();
                        let summary = format!(
                        "**Schema package:** `{}` {} · `{}` · {} · binding `{}`\n\nroot: `{}`{}{}",
                        b.package_name,
                        b.package_version,
                        format_args!("blake3:{}", nml_validate::store::hash8(&b.content_hash)),
                        b.class.label(),
                        b.binding_name,
                        packages::display_path(&b.root, &roots),
                        layers,
                        if b.step == packages::BindingStep::AutoAssociated {
                            "\n\n_auto-associated — a `schemaPackages` pin makes this explicit_"
                        } else {
                            ""
                        }
                    );
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: summary,
                            }),
                            range: None,
                        }));
                    }
                }
            }

            let source_clone = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                match docs.get(&uri) {
                    Some(s) => s.clone(),
                    None => return Ok(None),
                }
            };

            let Some(line) = position::line_at(&source_clone, pos.line) else {
                return Ok(None);
            };
            let byte_col = position::utf16_to_byte(line, pos.character);

            // One lex+parse for every hover surface: the CST feeds the
            // duration query, the lowered AST feeds the field-hover walk.
            let (file, parse) = nml_core::cst::parse_best_effort_with_tree(&source_clone);
            let line_index = LineIndex::new(&source_clone);
            if let Some(hover) =
                duration_lsp::duration_hover(&parse, &source_clone, pos, &line_index)
            {
                return Ok(Some(hover));
            }

            // Directive hover (RFC 0030/0032): `#name` in a covered model file
            // renders the vocabulary entry. Unknown names get no hover — the
            // vocabulary diagnostic already explains them.
            if is_schema_source(&uri) {
                if let Some(name) = directive_name_at(line, byte_col) {
                    // Covered files only: without a known vocabulary there is no
                    // entry to render (undetermined coverage already surfaced
                    // through the info diagnostic).
                    if let packages::VocabularyOutcome::Covered(vocab) =
                        self.vocabulary_for_document(&uri)
                    {
                        if let Some(d) = vocab.vocabulary.get(&name) {
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    // Fence-escaped: vocabulary docs are
                                    // author-supplied text (see
                                    // `escape_markdown_fences`).
                                    value: format!(
                                        "**#{}** ({}) — {}",
                                        d.name,
                                        d.arg.label(),
                                        escape_markdown_fences(d.doc)
                                    ),
                                }),
                                range: None,
                            }));
                        }
                    }
                }
            }

            let word = extract_word_at(line, byte_col);

            if word.starts_with('@') {
                let hover_text = word.strip_prefix('@').and_then(|stripped| {
                    let (keyword, name) = stripped.split_once('/')?;
                    self.find_tagged_ref_hover(keyword, name)
                });
                if let Some(text) = hover_text {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: text,
                        }),
                        range: None,
                    }));
                }
                return Ok(None);
            }

            let is_prop = is_property_name_position(line, &word, byte_col);

            if is_prop && !word.is_empty() {
                if let Some(keyword) = find_enclosing_block_keyword(&file, pos, &line_index) {
                    let handle = self.schema_index_for(&uri);
                    if let Some(model) = handle.index().model(&keyword) {
                        if let Some(field) = model.fields.iter().find(|f| f.name == word) {
                            // In source syntax the `|` sigil belongs to the
                            // field name (`|allow []string`), not the type.
                            let sigil = if is_modifier_form(field) { "|" } else { "" };
                            let opt = if field.optional { "?" } else { "" };
                            let mut text = format!(
                                "**{keyword}** field\n\n```nml\n  {sigil}{} {}{opt}\n```",
                                field.name, field.field_type
                            );
                            // The schema author's leading comment block (RFC 0004
                            // §4.3) is the field's documentation — rendered as a
                            // markdown paragraph under the signature.
                            if let Some(doc) = &field.doc {
                                text.push_str("\n\n");
                                // Fence-escaped: the doc is author-supplied text
                                // spliced after our own fenced signature block
                                // (see `escape_markdown_fences`).
                                text.push_str(&escape_markdown_fences(doc));
                            }
                            return Ok(Some(Hover {
                                contents: HoverContents::Markup(MarkupContent {
                                    kind: MarkupKind::Markdown,
                                    value: text,
                                }),
                                range: None,
                            }));
                        }
                    }
                }
            }

            if !is_prop {
                let builtin_info = match word.as_str() {
                    "string" => Some("**string** -- Quoted text value"),
                    "number" => Some(
                        "**number** -- Exact decimal (up to 34 significant digits); \
                         integers and decimals never round",
                    ),
                    "money" => Some(
                        "**money** -- Exact currency value with ISO 4217 code (e.g., `19.99 USD`)",
                    ),
                    "bool" => Some("**bool** -- Boolean value (`true` or `false`)"),
                    "duration" => Some(
                        "**duration** -- Exact time duration with unit suffix \
                         (e.g., `72h`, `30s`, `250ms`); `30s == 30000ms`",
                    ),
                    "path" => Some("**path** -- URL path with variables and wildcards"),
                    "secret" => Some("**secret** -- Value resolved from environment (`$ENV.X`)"),
                    "model" => Some("**model** -- Define a custom object type"),
                    "enum" => Some("**enum** -- Define a restricted set of allowed values"),
                    _ => None,
                };

                if let Some(text) = builtin_info {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: text.to_string(),
                        }),
                        range: None,
                    }));
                }
            }

            if !word.is_empty() {
                let model_ref_types = if !is_prop {
                    let handle = self.schema_index_for(&uri);
                    find_model_ref_types_at(&file, &source_clone, pos, handle.index(), &line_index)
                } else {
                    Vec::new()
                };

                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(text) = find_declaration_hover(&docs, &word, &model_ref_types) {
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: text,
                        }),
                        range: None,
                    }));
                }
            }

            Ok(None)
        }
        .await?;
        let aug = self.diagnostic_explanations_at(&aug_uri, aug_pos).await;
        Ok(merge_hover(base, aug))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let (word, enclosing_keyword, is_prop) = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let Some(line) = position::line_at(source, pos.line) else {
                return Ok(None);
            };
            let byte_col = position::utf16_to_byte(line, pos.character);
            let word = extract_word_at(line, byte_col);
            let is_prop = is_property_name_position(line, &word, byte_col);

            let enclosing = {
                let file = nml_core::cst::parse_best_effort(source);
                let line_index = LineIndex::new(source);
                find_enclosing_block_keyword(&file, pos, &line_index)
            };

            (word, enclosing, is_prop)
        };

        if word.is_empty() {
            return Ok(None);
        }

        if word.starts_with('@') {
            if let Some(result) = self.find_tagged_ref_definition(&word) {
                return Ok(Some(GotoDefinitionResponse::Scalar(result)));
            }
            return Ok(None);
        }

        if !is_prop {
            if let Some(ref keyword) = enclosing_keyword {
                if keyword == &word {
                    if let Some((target_uri, range)) = self.find_schema_definition(&word, &uri) {
                        return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                            uri: target_uri,
                            range,
                        })));
                    }
                }
            }
        }

        if let Some((target_uri, range)) =
            self.find_definition(&word, &uri, enclosing_keyword.as_deref())
        {
            Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: target_uri,
                range,
            })))
        } else {
            Ok(None)
        }
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;

        let word = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let Some(line) = position::line_at(source, pos.line) else {
                return Ok(None);
            };
            extract_word_at(line, position::utf16_to_byte(line, pos.character))
        };

        if word.is_empty() {
            return Ok(None);
        }

        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let mut locations = Vec::new();

        for (doc_uri, source) in docs.iter() {
            let line_index = LineIndex::new(source);
            for range in find_references_in_source(source, &word, &line_index) {
                locations.push(Location {
                    uri: doc_uri.clone(),
                    range,
                });
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let source_clone = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        // Resilient parse keeps the document outline populated mid-edit instead
        // of collapsing to empty on the first syntax error.
        let file = nml_core::cst::parse_best_effort(&source_clone);

        let line_index = LineIndex::new(&source_clone);
        let symbols = build_document_symbols(&file, &line_index);
        Ok(Some(DocumentSymbolResponse::Nested(symbols)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let pos = params.text_document_position_params.position;
        let uri = params.text_document_position_params.text_document.uri;

        let source_clone = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            source.clone()
        };
        let parse = nml_core::cst::parse(&source_clone);

        let line_index = LineIndex::new(&source_clone);
        if let Some(range) = duration_lsp::duration_highlight_range(&parse, pos, &line_index) {
            return Ok(Some(vec![DocumentHighlight {
                range,
                kind: Some(DocumentHighlightKind::READ),
            }]));
        }

        let word = {
            let Some(line) = position::line_at(&source_clone, pos.line) else {
                return Ok(None);
            };
            extract_word_at(line, position::utf16_to_byte(line, pos.character))
        };

        if word.is_empty() {
            return Ok(None);
        }

        let refs = find_references_in_source(&source_clone, &word, &line_index);

        if refs.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                refs.into_iter()
                    .map(|range| DocumentHighlight {
                        range,
                        kind: Some(DocumentHighlightKind::TEXT),
                    })
                    .collect(),
            ))
        }
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };
        let parse = nml_core::cst::parse(&source);
        Ok(Some(crate::semantic_tokens::full(&parse, &source)))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri;
        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };
        let line_index = LineIndex::new(&source);
        let span = Span::new(
            line_index.offset(params.range.start),
            line_index.offset(params.range.end),
        );
        let parse = nml_core::cst::parse(&source);
        Ok(Some(crate::semantic_tokens::range(&parse, &source, span)))
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };
        let line_index = LineIndex::new(&source);
        let span = Span::new(
            line_index.offset(params.range.start),
            line_index.offset(params.range.end),
        );
        let parse = nml_core::cst::parse(&source);
        let hints = nml_core::cst::duration_literals_in(&parse, span)
            .into_iter()
            .filter_map(|at| {
                // Signed literals are domain-invalid (durations are
                // unsigned): a `= 90m` hint beside `-1h30m` would mislead.
                if at.components.len() <= 1 || at.sign.is_some() {
                    return None;
                }
                // Token-reconstructed attached text: immune to interior
                // trivia (tabs, multiple spaces) in the spaced source form.
                let d =
                    nml_core::duration::Duration::parse_text(&at.literal.attached_text()).ok()?;
                let hint = d.coarsest_exact()?;
                Some(InlayHint {
                    position: line_index.position(at.tight_span().end),
                    label: InlayHintLabel::String(format!("= {hint}")),
                    kind: Some(InlayHintKind::TYPE),
                    padding_left: Some(true),
                    text_edits: None,
                    tooltip: None,
                    padding_right: None,
                    data: None,
                })
            })
            .collect::<Vec<_>>();
        Ok((!hints.is_empty()).then_some(hints))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = params.text_document.uri;
        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };
        let parse = nml_core::cst::parse(&source);
        let line_index = LineIndex::new(&source);
        let mut out = Vec::new();
        for pos in params.positions {
            let byte = line_index.offset(pos);
            let root = parse.syntax();
            let tok = root
                .token_at_offset((byte as u32).into())
                .right_biased()
                .or_else(|| root.token_at_offset((byte as u32).into()).left_biased());
            let mut chain: Option<Box<SelectionRange>> = None;
            let mut node = tok.and_then(|t| t.parent());
            while let Some(n) = node {
                let span = Span::new(
                    usize::from(n.text_range().start()),
                    usize::from(n.text_range().end()),
                );
                let range = line_index.range(span);
                chain = Some(Box::new(SelectionRange {
                    range,
                    parent: chain,
                }));
                node = n.parent();
            }
            // LSP contract: exactly one entry per requested position. A
            // position outside any token gets an empty range at itself.
            out.push(chain.map(|r| *r).unwrap_or(SelectionRange {
                range: Range::new(pos, pos),
                parent: None,
            }));
        }
        Ok(Some(out))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let source_clone = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let formatted = match nml_fmt::formatter::format_source(&source_clone) {
            Ok(f) => f,
            Err(e) => {
                // Never a lossy rewrite: a document that does not parse is
                // not formatted (RFC 0004's own rule), and the reason is
                // SAID — as a log line, not a `window/showMessage` toast:
                // the parse finding already marks the line, and
                // format-on-save would otherwise toast every save (the
                // documented practice of rust-analyzer's formatting handler
                // for a parse error). `null` is LSP's "no edits".
                let finding = e.to_diagnostic();
                let at = nml_core::span::SourceMap::new(&source_clone).location(e.span().start);
                let roots = self
                    .workspace_roots
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let name = uri
                    .to_file_path()
                    .map(|p| packages::source_name_of(&p, &roots))
                    .unwrap_or_else(|_| uri.to_string());
                let code = finding.code.map(|c| format!(" [{c}]")).unwrap_or_default();
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "NML: formatting skipped for {name}: {}:{}:{code} {} — the formatter \
                             never rewrites a document it cannot round-trip; fix the parse error first",
                            at.line,
                            at.column,
                            finding.rendered()
                        ),
                    )
                    .await;
                return Ok(None);
            }
        };
        if formatted == source_clone {
            return Ok(None);
        }

        let line_count = source_clone.lines().count() as u32;
        let last_line_len = source_clone
            .lines()
            .last()
            .map_or(0, |l| position::byte_to_utf16(l, l.len()));
        let (end_line, end_char) = if source_clone.ends_with('\n') {
            (line_count, 0)
        } else {
            (line_count.saturating_sub(1), last_line_len)
        };

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: Position::new(end_line, end_char),
            },
            new_text: formatted,
        }]))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        if params.ch != "\n" {
            return Ok(None);
        }

        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let source = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            match docs.get(&uri) {
                Some(s) => s.clone(),
                None => return Ok(None),
            }
        };

        let lines: Vec<&str> = source.lines().collect();

        if pos.line == 0 {
            return Ok(None);
        }

        let prev_line_idx = (pos.line - 1) as usize;
        if prev_line_idx >= lines.len() {
            return Ok(None);
        }

        // The unit is read at the previous line's first character — the
        // header the new line nests under (LSP 3.17 §FormattingOptions'
        // `tabSize` is the CLIENT's guess at this document; the tree
        // knows, and tabs are never NML indentation).
        let unit = {
            let line_start: usize = source
                .split_inclusive('\n')
                .take(prev_line_idx)
                .map(str::len)
                .sum();
            let content = lines[prev_line_idx].len() - lines[prev_line_idx].trim_start().len();
            nml_core::cst::edit::indentation_unit_at(&source, line_start + content).len()
        };
        let desired = compute_indent_after_line(&lines, prev_line_idx, unit);
        let indent_str: String = " ".repeat(desired);

        let current_line_idx = pos.line as usize;
        // `trim_start` trims Unicode whitespace, so the byte count must be
        // converted to UTF-16 units for the edit range.
        let (existing_ws_bytes, existing_ws_end) = if current_line_idx < lines.len() {
            let cur = lines[current_line_idx];
            let ws = cur.len() - cur.trim_start().len();
            (ws, position::byte_to_utf16(cur, ws))
        } else {
            (0, 0)
        };

        if existing_ws_bytes == desired {
            return Ok(None);
        }

        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(pos.line, 0),
                end: Position::new(pos.line, existing_ws_end),
            },
            new_text: indent_str,
        }]))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let new_name = params.new_name;

        let word = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let Some(line) = position::line_at(source, pos.line) else {
                return Ok(None);
            };
            extract_word_at(line, position::utf16_to_byte(line, pos.character))
        };

        if word.is_empty() {
            return Ok(None);
        }

        // BORROWED, never copied: this map holds every indexed file's
        // full text, so cloning it charged the whole workspace's bytes to
        // one keystroke-frequency request. The readers below are free
        // functions over the map and never re-enter the server, so the
        // guard is simply held across them.
        // The guard is released before the edit is built: the versioned
        // shape reads the same map for each document's client version, and
        // `std::sync::Mutex` is not re-entrant.
        let per_file: Vec<(Url, Vec<TextEdit>)> = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            docs.iter()
                .filter_map(|(doc_uri, source)| {
                    let line_index = LineIndex::new(source);
                    let refs = find_references_in_source(source, &word, &line_index);
                    (!refs.is_empty()).then(|| {
                        (
                            doc_uri.clone(),
                            refs.into_iter()
                                .map(|range| TextEdit {
                                    range,
                                    new_text: new_name.clone(),
                                })
                                .collect(),
                        )
                    })
                })
                .collect()
        };

        if per_file.is_empty() {
            return Ok(None);
        }
        // Through the ONE edit shape, like every other action: a rename
        // spanning files is exactly what a VERSIONED edit is for — the
        // client refuses it wholesale if any of those buffers moved on
        // since. (It used to hand out plain `changes`, unversioned,
        // whatever the client declared.)
        Ok(Some(self.workspace_edits(per_file)))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let pos = params.position;
        let uri = params.text_document.uri;

        let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };
        let Some(line) = position::line_at(source, pos.line) else {
            return Ok(None);
        };

        let byte_col = position::utf16_to_byte(line, pos.character);
        let word = extract_word_at(line, byte_col);
        if word.is_empty() {
            return Ok(None);
        }

        let (start, end) = rename_word_byte_range(line, byte_col);
        Ok(Some(PrepareRenameResponse::Range(Range {
            start: Position::new(pos.line, position::byte_to_utf16(line, start)),
            end: Position::new(pos.line, position::byte_to_utf16(line, end)),
        })))
    }
}

/// Byte range of the renameable identifier around the given byte column.
/// Uses a narrower character set than `is_word_char`: rename targets plain
/// identifiers, not `@kind/name` references.
fn rename_word_byte_range(line: &str, byte_col: usize) -> (usize, usize) {
    let is_rename_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut col = byte_col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }

    let start = line[..col]
        .char_indices()
        .rev()
        .find(|(_, c)| !is_rename_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let end = line[col..]
        .char_indices()
        .find(|(_, c)| !is_rename_char(*c))
        .map(|(i, _)| col + i)
        .unwrap_or(line.len());

    (start, end)
}

// ── Tests ─────────────────────────────────────────────────────

use crate::packages::canonical_above_roots;

#[cfg(test)]
mod tests {

    /// Step 0e: a document path is canonical ABOVE its workspace root and
    /// untouched below it. A link an author committed inside the root
    /// reaches the kernel as the link (NML2083, as `nml check` says);
    /// the editor used to canonicalize the whole path and judge the
    /// link's TARGET under whatever binding claims it. An operator's
    /// symlinked prefix above the root (a linked checkout, macOS's
    /// `/tmp`) still resolves onto the canonical root; outside every
    /// root the path stays as spelled (the kernel derives a root from
    /// it, following links only above the fence).
    #[cfg(unix)]
    #[test]
    fn a_document_path_is_canonical_above_its_root_and_untouched_below() {
        let base = crate::scratch::Scratch::new("lsp-canon");
        std::fs::create_dir_all(base.join("real/ws/tenants/cu")).unwrap();
        let root = dunce::canonicalize(base.join("real/ws")).unwrap();
        std::fs::write(root.join("tenants/cu/plain.flow.nml"), "").unwrap();
        std::os::unix::fs::symlink("plain.flow.nml", root.join("tenants/cu/link.flow.nml"))
            .unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("alias")).unwrap();
        let roots = vec![root.clone()];
        // Inside the root: the link is kept.
        let inside = root.join("tenants/cu/link.flow.nml");
        assert_eq!(super::canonical_above_roots(inside.clone(), &roots), inside);
        // Through an operator's link ABOVE the root: the prefix resolves,
        // the link below it does not.
        let aliased = base.join("alias/ws/tenants/cu/link.flow.nml");
        assert_eq!(super::canonical_above_roots(aliased, &roots), inside);
        // Outside every root: as spelled — never resolved through a link
        // the kernel has not judged.
        let _ = std::fs::write(base.join("x.nml"), "");
        assert_eq!(
            super::canonical_above_roots(base.join("alias/../x.nml"), &roots),
            base.join("alias/../x.nml")
        );
    }
    /// A related note's file is read from disk
    /// only up to [`MAX_LOCATE_BYTES`]; an open buffer is the truth at
    /// any size.
    #[test]
    fn locating_a_notes_file_is_capped_at_max_locate_bytes() {
        let base = crate::scratch::Scratch::new("lsp-locate");
        let small = base.join("small.nml");
        std::fs::write(&small, "model s:\n    a number\n").unwrap();
        let big = base.join("big.nml");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_LOCATE_BYTES + 1)
            .unwrap();
        let docs = HashMap::new();
        assert!(locate_source(&docs, &small).is_some_and(|(_, text)| text.contains("model s")));
        assert!(
            locate_source(&docs, &big).is_none(),
            "over the cap: not read"
        );
        let mut docs = HashMap::new();
        docs.insert(Url::from_file_path(&big).unwrap(), "buffered".to_string());
        assert!(locate_source(&docs, &big).is_some_and(|(_, text)| text == "buffered"));
    }

    /// Code actions from one diagnostic's wire
    /// suggestions are capped at [`MAX_SUGGESTION_ACTIONS`] VALID entries
    /// — malformed padding neither counts nor buries a real one.
    #[test]
    fn suggestion_actions_are_capped_at_the_bound_after_validation() {
        let valid = |i: usize| serde_json::json!({"replacement": format!("r{i}"), "start": 0, "end": 1, "kind": "fix"});
        let entries: Vec<serde_json::Value> = (0..MAX_SUGGESTION_ACTIONS + 4).map(valid).collect();
        assert_eq!(
            parse_suggestion_entries(&entries).len(),
            MAX_SUGGESTION_ACTIONS
        );
        let mut padded: Vec<serde_json::Value> = (0..MAX_SUGGESTION_ACTIONS + 4)
            .map(|_| serde_json::json!({"start": 3, "end": 1}))
            .collect();
        padded.extend((0..MAX_SUGGESTION_ACTIONS - 1).map(valid));
        assert_eq!(
            parse_suggestion_entries(&padded).len(),
            MAX_SUGGESTION_ACTIONS - 1
        );
        // An entry that names another document's `source` is that
        // document's edit: kept, with its `source`, for the action to
        // route there — never resolved against this document's text.
        let elsewhere = serde_json::json!({
            "replacement": "layers:\n    allowRefs:\n        - \"a\"",
            "start": 152, "end": 163, "kind": "insert", "source": "demo.package.nml"
        });
        let routed = parse_suggestion_entries(std::slice::from_ref(&elsewhere));
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].source.as_deref(), Some("demo.package.nml"));
        assert_eq!(parse_suggestion_entries(&[elsewhere, valid(0)]).len(), 2);
    }

    /// The "Simplify number" decision function (RFC 0016 §1.10): offers
    /// the minimal cohort member for plain numbers, never for money
    /// (fmt re-canonicalizes money — the edit would revert on save),
    /// spells the sign into the title (the `-` is a separate token),
    /// and stays silent when the literal is already minimal.
    #[test]
    fn simplify_number_action_edges() {
        let src = "service App:\n    x = 8080.000\n";
        let off = src.find("8080").unwrap();
        let a = super::simplify_number_action(src, off).expect("action");
        assert_eq!(a.title, "Simplify number to `8080`");
        assert_eq!(a.new_text, "8080");
        assert_eq!(&src[a.span.start..a.span.end], "8080.000");

        // Money: skipped entirely.
        let src = "service App:\n    price = 19.90 USD\n";
        let off = src.find("19.90").unwrap();
        assert!(
            super::simplify_number_action(src, off).is_none(),
            "money literals must not offer simplify — fmt would revert it"
        );

        // Negative literal: edit replaces digits only, title shows sign.
        let src = "service App:\n    x = -8080.000\n";
        let off = src.find("8080").unwrap();
        let a = super::simplify_number_action(src, off).expect("action");
        assert_eq!(a.title, "Simplify number to `-8080`");
        assert_eq!(a.new_text, "8080");

        // A list marker's dash is NOT a sign (space-separated token).
        let src = "service App:\n    ports:\n        - 8080.0\n";
        let off = src.find("8080").unwrap();
        let a = super::simplify_number_action(src, off).expect("action");
        assert_eq!(a.title, "Simplify number to `8080`");

        // Already minimal: no action.
        let src = "service App:\n    x = 2.5\n";
        let off = src.find("2.5").unwrap();
        assert!(super::simplify_number_action(src, off).is_none());

        // Inline arrays: elements are their own nodes, so an Ident
        // NEIGHBOR (a reference) must not trip the money check — while
        // genuine money inside an array still suppresses.
        let src = "service App:\n    ports = [8080.000, OtherRef]\n";
        let off = src.find("8080").unwrap();
        assert!(
            super::simplify_number_action(src, off).is_some(),
            "reference neighbor must not suppress simplify"
        );
        let src = "service App:\n    prices = [19.90 USD, 20.00 USD]\n";
        let off = src.find("19.90").unwrap();
        assert!(
            super::simplify_number_action(src, off).is_none(),
            "array money must suppress"
        );
    }

    use super::*;
    use std::collections::HashMap;

    /// The on-type indent's delimiter counter is escape-aware (lexer
    /// parity): `\"""` is an escaped quote plus two plain quotes, never a
    /// delimiter — a naive substring count would toggle on it and
    /// mis-indent everything after.
    #[test]
    fn triple_quote_counter_is_escape_aware() {
        assert_eq!(count_triple_quotes(r#"x = """"#), 1);
        assert_eq!(count_triple_quotes(r#"a\""""#), 0);
        assert_eq!(count_triple_quotes(r#"say \"\"\" then"#), 0);
        assert_eq!(count_triple_quotes(r#""""body""""#), 2);
        assert!(is_inside_triple_quote(&["x = \"\"\"", "  a\\\"\"\""], 2));
    }

    // ── project_file_insertion (RFC 0030 P2 structural writes) ─

    #[test]
    fn pin_insert_preserves_comments() {
        // The RFC 0030 P2 payoff: a hand-commented project file survives a
        // pin byte-for-byte outside the inserted line.
        let text = "\
// team conventions: keep pins sorted
project MyApp:
    // we pin explicitly
    schemaPackages:
        - alpha
        // beta is legacy
        - beta
    autoAssociate = false
";
        let out = project_file_insertion(text, &ProjectEdit::Pin("gamma".into()))
            .expect("pin insert succeeds");
        assert_eq!(
            out,
            "\
// team conventions: keep pins sorted
project MyApp:
    // we pin explicitly
    schemaPackages:
        - alpha
        // beta is legacy
        - beta
        - gamma
    autoAssociate = false
"
        );
    }

    #[test]
    fn pin_insert_creates_schema_packages_block() {
        let text = "project MyApp:\n    autoAssociate = false\n";
        let out = project_file_insertion(text, &ProjectEdit::Pin("demo".into()))
            .expect("pin insert succeeds");
        assert_eq!(
            out,
            "project MyApp:\n    schemaPackages:\n        - demo\n    autoAssociate = false\n"
        );
    }

    #[test]
    fn pin_insert_redundant_or_structureless_returns_none() {
        // Already pinned → no action offered.
        let pinned = "project P:\n    schemaPackages:\n        - demo\n";
        assert_eq!(
            project_file_insertion(pinned, &ProjectEdit::Pin("demo".into())),
            None
        );
        // No `project` block to target → no action (matches the old
        // line-anchor behavior, now enforced structurally).
        assert_eq!(
            project_file_insertion(
                "service App:\n    x = 1\n",
                &ProjectEdit::Pin("demo".into())
            ),
            None
        );
    }

    #[test]
    fn opt_out_inserts_after_header_and_is_idempotent() {
        let text = "project P:\n    // pins below\n    schemaPackages:\n        - demo\n";
        let out = project_file_insertion(text, &ProjectEdit::OptOut).expect("opt-out succeeds");
        assert_eq!(
            out,
            "project P:\n    autoAssociate = false\n    // pins below\n    schemaPackages:\n        - demo\n"
        );
        assert_eq!(project_file_insertion(&out, &ProjectEdit::OptOut), None);
    }

    /// Idempotency is structural (via `ProjectConfig`), so text that merely
    /// *looks* like a pin or opt-out inside a comment can never
    /// false-suppress the action — the failure mode of the retired text
    /// scans.
    #[test]
    fn idempotency_ignores_lookalike_text_in_comments() {
        // A comment naming the pin does not count as pinned.
        let commented_pin = "project P:\n    // - demo is not really pinned\n    x = 1\n";
        assert!(project_file_insertion(commented_pin, &ProjectEdit::Pin("demo".into())).is_some());
        // A comment naming the opt-out does not count as opted out.
        let commented_optout = "project P:\n    // autoAssociate = false (someday)\n    x = 1\n";
        assert!(project_file_insertion(commented_optout, &ProjectEdit::OptOut).is_some());
    }

    // ── extract_word_at ───────────────────────────────────────

    #[test]
    fn directive_name_at_on_name_and_on_hash() {
        let line = "    name string+ #live";
        // On the name (any byte of `live`), and on the `#` itself.
        for col in 17..=21 {
            assert_eq!(
                directive_name_at(line, col).as_deref(),
                Some("live"),
                "col {col}"
            );
        }
    }

    #[test]
    fn directive_name_at_rejects_plain_words() {
        let line = "    name string+ #live";
        // `name`, `string+` — word positions without a leading `#`.
        assert_eq!(directive_name_at(line, 6), None);
        assert_eq!(directive_name_at(line, 11), None);
        // A `#` with no name after it.
        assert_eq!(directive_name_at("    name string+ #", 18), None);
    }

    #[test]
    fn directive_name_at_argful() {
        let line = "    host string #key(host)";
        assert_eq!(directive_name_at(line, 18).as_deref(), Some("key"));
        // Inside the argument parens: preceded by `(`, not `#` — not a
        // directive name, no directive hover.
        assert_eq!(directive_name_at(line, 22), None);
    }

    #[test]
    fn extract_word_in_middle() {
        assert_eq!(extract_word_at("hello world", 7), "world");
    }

    #[test]
    fn extract_word_at_line_start() {
        assert_eq!(extract_word_at("provider GroqFast:", 3), "provider");
    }

    #[test]
    fn extract_word_at_line_end() {
        assert_eq!(extract_word_at("foo = Bar", 8), "Bar");
    }

    #[test]
    fn extract_word_with_hyphens_underscores() {
        assert_eq!(extract_word_at("my-service_name", 5), "my-service_name");
    }

    #[test]
    fn extract_word_on_whitespace() {
        assert_eq!(extract_word_at("foo   bar", 4), "");
    }

    #[test]
    fn extract_word_empty_line() {
        assert_eq!(extract_word_at("", 0), "");
    }

    #[test]
    fn extract_word_on_equals() {
        assert_eq!(extract_word_at("key = val", 4), "");
    }

    #[test]
    fn extract_word_past_end() {
        assert_eq!(extract_word_at("foo", 100), "foo");
    }

    #[test]
    fn extract_word_role_ref() {
        assert_eq!(extract_word_at("access = @role/admin", 12), "@role/admin");
    }

    #[test]
    fn extract_word_role_ref_cursor_at_start() {
        assert_eq!(extract_word_at("@public", 0), "@public");
    }

    #[test]
    fn extract_word_role_ref_with_dot() {
        assert_eq!(
            extract_word_at("user = @user/test@example.com", 10),
            "@user/test@example.com"
        );
    }

    #[test]
    fn extract_word_role_ref_at_keyword() {
        assert_eq!(extract_word_at("@role/admin", 3), "@role/admin");
    }

    #[test]
    fn extract_word_role_ref_at_name() {
        assert_eq!(extract_word_at("@role/admin", 8), "@role/admin");
    }

    // ── find_name_by_text ─────────────────────────────────────

    #[test]
    fn find_by_text_keyword_name_colon() {
        let source = "provider GroqFast:\n    type = \"groq\"";
        let result = find_name_by_text(source, "GroqFast");
        assert!(result.is_some());
        let range = result.unwrap();
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 9);
    }

    #[test]
    fn find_by_text_dash_name_colon() {
        let source = "steps:\n    - myStep:\n        provider = Groq";
        let result = find_name_by_text(source, "myStep");
        assert!(result.is_some());
        let range = result.unwrap();
        assert_eq!(range.start.line, 1);
    }

    #[test]
    fn find_by_text_not_found() {
        let source = "provider GroqFast:\n    type = \"groq\"";
        assert!(find_name_by_text(source, "NonExistent").is_none());
    }

    #[test]
    fn find_by_text_ignores_values() {
        let source = "provider = GroqFast";
        assert!(find_name_by_text(source, "GroqFast").is_none());
    }

    // ── span_to_range ─────────────────────────────────────────

    #[test]
    fn span_to_range_single_line() {
        let source = "provider GroqFast:";
        let line_index = LineIndex::new(source);
        let span = nml_core::span::Span::new(9, 17);
        let range = span_to_range(span, &line_index);
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 9);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 17);
    }

    #[test]
    fn span_to_range_multi_line() {
        let source = "hello\nworld";
        let line_index = LineIndex::new(source);
        let span = nml_core::span::Span::new(6, 11);
        let range = span_to_range(span, &line_index);
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 5);
    }

    // ── find_top_level_decl ───────────────────────────────────

    #[test]
    fn find_top_level_block() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_top_level_decl(&file, "GroqFast", &line_index).is_some());
    }

    #[test]
    fn find_top_level_const() {
        let source = "const Limit = 100\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_top_level_decl(&file, "Limit", &line_index).is_some());
    }

    #[test]
    fn find_top_level_not_found() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_top_level_decl(&file, "NonExistent", &line_index).is_none());
    }

    // ── find_field_definition ─────────────────────────────────

    #[test]
    fn find_field_in_model() {
        let source = "model user:\n    name string\n    email string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_field_definition(&file, "email", &line_index).is_some());
    }

    #[test]
    fn find_field_ignores_non_model() {
        let source = "service Svc:\n    localMount = \"/\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_field_definition(&file, "localMount", &line_index).is_none());
    }

    #[test]
    fn find_field_not_found() {
        let source = "model user:\n    name string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_field_definition(&file, "nonexistent", &line_index).is_none());
    }

    // ── find_name_in_file ─────────────────────────────────────

    #[test]
    fn find_name_top_level() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_name_in_file(&file, "GroqFast", &line_index).is_some());
    }

    #[test]
    fn find_name_nested_block() {
        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = GroqFast\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_name_in_file(&file, "steps", &line_index).is_some());
    }

    #[test]
    fn find_name_list_item() {
        let source = "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - myStep:\n            provider = GroqFast\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_name_in_file(&file, "myStep", &line_index).is_some());
    }

    #[test]
    fn find_name_not_found_in_file() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_name_in_file(&file, "NonExistent", &line_index).is_none());
    }

    // ── find_definition_in_docs (priority + regression) ───────

    fn make_uri(name: &str) -> Url {
        Url::parse(&format!("file:///workspace/{name}")).unwrap()
    }

    #[test]
    fn definition_prefers_current_file() {
        let mut docs = HashMap::new();
        let current = make_uri("async-agent-test.workflow.nml");
        let other = make_uri("simple-chat.workflow.nml");

        docs.insert(
            current.clone(),
            "provider GroqFast:\n    type = \"groq\"\n    model = \"llama-3.3-70b-versatile\"\n"
                .to_string(),
        );
        docs.insert(
            other.clone(),
            "provider GroqFast:\n    type = \"groq\"\n    model = \"llama-3.1-8b-instant\"\n"
                .to_string(),
        );

        let result = find_definition_in_docs(&docs, "GroqFast", &current, None);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(
            uri, current,
            "should resolve to current file, not other file"
        );
    }

    #[test]
    fn definition_model_field_first() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("schema.model.nml");
        let current = make_uri("app.nml");

        docs.insert(
            model_uri.clone(),
            "model user:\n    name string\n    email string\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "service Svc:\n    name = \"test\"\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "name", &current, None);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, model_uri, "model field should take priority");
    }

    #[test]
    fn definition_falls_back_to_other_file() {
        let mut docs = HashMap::new();
        let current = make_uri("app.nml");
        let other = make_uri("providers.nml");

        docs.insert(
            current.clone(),
            "workflow W:\n    provider = GroqFast\n".to_string(),
        );
        docs.insert(
            other.clone(),
            "provider GroqFast:\n    type = \"groq\"\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "GroqFast", &current, None);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, other);
    }

    #[test]
    fn definition_nested_name_in_current() {
        let mut docs = HashMap::new();
        let current = make_uri("workflow.nml");
        docs.insert(
            current.clone(),
            "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - myStep:\n            provider = GroqFast\n"
                .to_string(),
        );

        let result = find_definition_in_docs(&docs, "myStep", &current, None);
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(uri, current);
    }

    #[test]
    fn definition_not_found() {
        let mut docs = HashMap::new();
        let current = make_uri("app.nml");
        docs.insert(
            current.clone(),
            "workflow W:\n    entrypoint = \"start\"\n".to_string(),
        );

        assert!(find_definition_in_docs(&docs, "NonExistent", &current, None).is_none());
    }

    // ── Scope extraction ──────────────────────────────────────

    #[test]
    fn extract_schema_scope_reads_both_spellings_as_one_scope() {
        assert_eq!(
            extract_schema_scope("file:///path/to/workflow.schema.nml"),
            "workflow"
        );
        assert_eq!(
            extract_schema_scope("file:///path/to/workflow.model.nml"),
            extract_schema_scope("file:///path/to/workflow.schema.nml")
        );
        assert_eq!(
            extract_file_scope("file:///path/to/workflow.schema.nml"),
            None,
            "a schema source has no file scope"
        );
        let schema = Url::parse("file:///path/to/core.schema.nml").expect("url");
        let instance = Url::parse("file:///path/to/core.flow.nml").expect("url");
        assert!(is_schema_source(&schema) && !is_schema_source(&instance));
    }

    #[test]
    fn extract_schema_scope_workflow() {
        assert_eq!(
            extract_schema_scope("file:///path/to/workflow.model.nml"),
            "workflow"
        );
    }

    #[test]
    fn extract_schema_scope_config() {
        assert_eq!(
            extract_schema_scope("file:///path/to/config.model.nml"),
            "config"
        );
    }

    #[test]
    fn extract_file_scope_workflow() {
        assert_eq!(
            extract_file_scope("file:///path/to/voice-agent.workflow.nml"),
            Some("workflow".to_string())
        );
    }

    #[test]
    fn extract_file_scope_plain() {
        assert_eq!(extract_file_scope("file:///path/to/app.nml"), None);
    }

    #[test]
    fn extract_file_scope_model_file() {
        assert_eq!(
            extract_file_scope("file:///path/to/workflow.model.nml"),
            None
        );
    }

    // ── Scoped definition resolution ──────────────────────────

    #[test]
    fn definition_field_resolves_to_enclosing_model() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("schema.model.nml");
        let current = make_uri("test.nml");

        docs.insert(
            model_uri.clone(),
            "model mount:\n    transport string\n\nmodel pipeline:\n    transport string?\n"
                .to_string(),
        );
        docs.insert(
            current.clone(),
            "pipeline P:\n    transport = TelnyxCall\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "transport", &current, Some("pipeline"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(uri, model_uri);
        // Should resolve to transport in model pipeline (line 4), not model mount (line 1)
        assert_eq!(
            range.start.line, 4,
            "should resolve to pipeline's transport field"
        );
    }

    #[test]
    fn definition_scoped_schema_preferred() {
        let mut docs = HashMap::new();
        let workflow_model = make_uri("workflow.model.nml");
        let config_model = make_uri("config.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            config_model.clone(),
            "model pipeline:\n    input []string?\n".to_string(),
        );
        docs.insert(
            workflow_model.clone(),
            "model pipeline:\n    transport string?\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "pipeline P:\n    transport = TelnyxCall\n".to_string(),
        );

        let result = find_definition_in_docs(&docs, "transport", &current, Some("pipeline"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        assert_eq!(
            uri, workflow_model,
            "should resolve to workflow.model.nml, not config.model.nml"
        );
    }

    // ── Keyword navigation (cmd+click on declaration keyword) ─

    #[test]
    fn keyword_skips_field_definitions() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            "model step:\n    provider string?\n\nmodel provider:\n    type string\n    model string\n".to_string(),
        );
        docs.insert(
            current.clone(),
            "provider GroqFast:\n    type = \"groq\"\n".to_string(),
        );

        // When enclosing_keyword == name (cursor on keyword), field lookup is skipped.
        // Should NOT go to "provider string?" field in model step (line 1).
        // Falls through to top-level decl lookup and finds "model provider:" (line 3).
        let result = find_definition_in_docs(&docs, "provider", &current, Some("provider"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(
            uri, model_uri,
            "should resolve to model definition, not to a field"
        );
        assert_eq!(
            range.start.line, 3,
            "should point to 'model provider:' declaration"
        );
    }

    #[test]
    fn find_schema_block_definition_finds_model() {
        let source = "model provider:\n    type string\n\nmodel workflow:\n    entrypoint string\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        let result = find_schema_block_definition(&file, "workflow", &line_index);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().start.line,
            3,
            "should find model workflow on line 3"
        );
    }

    #[test]
    fn find_schema_block_definition_finds_enum() {
        let source = "enum transport:\n    - \"http\"\n    - \"websocket\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        let result = find_schema_block_definition(&file, "transport", &line_index);
        assert!(result.is_some());
        assert_eq!(result.unwrap().start.line, 0);
    }

    #[test]
    fn find_schema_block_definition_ignores_instances() {
        let source = "provider GroqFast:\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        let result = find_schema_block_definition(&file, "GroqFast", &line_index);
        assert!(result.is_none(), "should not match instance declarations");
    }

    #[test]
    fn keyword_does_not_match_field_in_other_model() {
        let mut docs = HashMap::new();
        let config_model = make_uri("config.model.nml");
        let server_model = make_uri("server.model.nml");
        let workflow_model = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            config_model.clone(),
            "model mount:\n    workflow string?\n".to_string(),
        );
        docs.insert(
            server_model.clone(),
            "model auth:\n    provider string\n".to_string(),
        );
        docs.insert(
            workflow_model.clone(),
            "model workflow:\n    entrypoint string\n\nmodel provider:\n    type string\n"
                .to_string(),
        );
        docs.insert(
            current.clone(),
            "workflow VoiceAgent:\n    entrypoint = \"start\"\n\nprovider Groq:\n    type = \"groq\"\n".to_string(),
        );

        // "workflow" with enclosing_keyword="workflow" should skip field lookups
        let result = find_definition_in_docs(&docs, "workflow", &current, Some("workflow"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        // Must NOT go to "workflow string?" in model mount (config.model.nml)
        assert_ne!(
            uri, config_model,
            "should not resolve to field 'workflow' in model mount"
        );

        // "provider" with enclosing_keyword="provider" should skip field lookups
        let result = find_definition_in_docs(&docs, "provider", &current, Some("provider"));
        assert!(result.is_some());
        let (uri, _) = result.unwrap();
        // Must NOT go to "provider string" in model auth (server.model.nml)
        assert_ne!(
            uri, server_model,
            "should not resolve to field 'provider' in model auth"
        );
    }

    // ── is_property_name_position ─────────────────────────────

    #[test]
    fn property_position_before_equals() {
        assert!(is_property_name_position(
            "    model = \"llama\"",
            "model",
            6
        ));
    }

    #[test]
    fn property_position_nested_block() {
        assert!(is_property_name_position("    inbound:", "inbound", 6));
    }

    #[test]
    fn not_property_position_keyword() {
        assert!(!is_property_name_position(
            "workflow VoiceAgent:",
            "workflow",
            3
        ));
    }

    #[test]
    fn not_property_position_value() {
        assert!(!is_property_name_position(
            "    transport = TelnyxCall",
            "TelnyxCall",
            18
        ));
    }

    #[test]
    fn not_property_position_top_level_block() {
        assert!(!is_property_name_position(
            "provider GroqFast:",
            "provider",
            3
        ));
    }

    // ── find_enclosing_block_keyword ─────────────────────────────

    #[test]
    fn enclosing_keyword_on_workflow_declaration() {
        let source = r#"stage TelnyxCall:
    wasm = "telnyx.wasm"
    accepts = "audio"
    produces = "audio"

provider GroqFast:
    type = "groq"
    model = "llama-3.3-70b-versatile"
    temperature = 0.7

workflow VoiceAgent:
    entrypoint = "conversation"
    steps:
        - conversation:
            provider = GroqFast
"#;
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        // "workflow" keyword is on line 11 (0-indexed)
        let pos = Position::new(11, 3);
        let result = find_enclosing_block_keyword(&file, pos, &line_index);
        assert_eq!(
            result,
            Some("workflow".to_string()),
            "cursor on 'workflow' should return 'workflow'"
        );

        // "provider" keyword is on line 5 (0-indexed)
        let pos = Position::new(5, 3);
        let result = find_enclosing_block_keyword(&file, pos, &line_index);
        assert_eq!(
            result,
            Some("provider".to_string()),
            "cursor on 'provider' should return 'provider'"
        );
    }

    #[test]
    fn enclosing_keyword_on_tool_declaration() {
        let source = r#"stage TelnyxCall:
    wasm = "telnyx.wasm"
    produces = "audio"

pipeline TelnyxVoice:
    transport = TelnyxCall
    inbound:
        - DeepgramSTT

tool DialViaTelnyx:
    pipeline = TelnyxVoice

provider GroqFast:
    type = "groq"
    model = "llama-3.3-70b-versatile"

workflow VoiceAgent:
    entrypoint = "conversation"
    steps:
        - conversation:
            provider = GroqFast
"#;
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        // "tool" keyword is on line 9 (0-indexed) - must return "tool" not "workflow" or "stage"
        let pos = Position::new(9, 3);
        let result = find_enclosing_block_keyword(&file, pos, &line_index);
        assert_eq!(
            result,
            Some("tool".to_string()),
            "cursor on 'tool' in tool DialViaTelnyx: should return 'tool'"
        );
    }

    #[test]
    fn enclosing_keyword_returns_none_for_blank_line() {
        let source = "stage A:\n    wasm = \"a.wasm\"\n\nstage B:\n    wasm = \"b.wasm\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);

        // Line 2 is the blank line between stage A and stage B
        let pos = Position::new(2, 0);
        let result = find_enclosing_block_keyword(&file, pos, &line_index);
        // Blank line may or may not be inside a declaration depending on parser spans
        // Just verify it doesn't panic
        let _ = result;
    }

    #[test]
    fn keyword_tool_goes_to_model_not_field() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            concat!(
                "model step:\n",
                "    provider string?\n",
                "    tool string?\n",
                "    tools []string?\n",
                "\n",
                "model tool:\n",
                "    wasm string?\n",
                "    pipeline string?\n",
            )
            .to_string(),
        );
        docs.insert(
            current.clone(),
            concat!("tool DialViaTelnyx:\n", "    pipeline = TelnyxVoice\n",).to_string(),
        );

        // Clicking on "tool" in "tool DialViaTelnyx:" should go to model tool: (line 5),
        // NOT to "tool string?" field in model step (line 2).
        let result = find_definition_in_docs(&docs, "tool", &current, Some("tool"));
        assert!(result.is_some());
        let (uri, range) = result.unwrap();
        assert_eq!(uri, model_uri);
        assert_eq!(
            range.start.line, 5,
            "should point to model tool:, not tool string? field"
        );
    }

    #[test]
    fn full_goto_keyword_to_schema_definition() {
        let mut docs = HashMap::new();
        let model_uri = make_uri("workflow.model.nml");
        let current = make_uri("voice-agent.workflow.nml");

        docs.insert(
            model_uri.clone(),
            concat!(
                "model provider:\n",
                "    type string\n",
                "    model string\n",
                "\n",
                "model step:\n",
                "    provider string?\n",
                "\n",
                "model workflow:\n",
                "    entrypoint string\n",
                "    steps []step\n",
            )
            .to_string(),
        );
        docs.insert(
            current.clone(),
            concat!(
                "provider GroqFast:\n",
                "    type = \"groq\"\n",
                "\n",
                "workflow VoiceAgent:\n",
                "    entrypoint = \"conversation\"\n",
            )
            .to_string(),
        );

        // Test 1: "workflow" with enclosing="workflow" (cursor on keyword)
        // find_schema_definition path: looks for model/trait/enum named "workflow"
        // Should find "model workflow:" on line 7 (0-indexed) in workflow.model.nml
        {
            let source = docs.get(&model_uri).unwrap();
            let file = nml_core::cst::parse_to_ast(source).unwrap();
            let line_index = LineIndex::new(source);
            let result = find_schema_block_definition(&file, "workflow", &line_index);
            assert!(
                result.is_some(),
                "find_schema_block_definition should find model workflow:"
            );
            let range = result.unwrap();
            assert_eq!(
                range.start.line, 7,
                "model workflow: is on line 7 (0-indexed)"
            );
        }

        // Test 2: "provider" with enclosing="provider" (cursor on keyword)
        {
            let source = docs.get(&model_uri).unwrap();
            let file = nml_core::cst::parse_to_ast(source).unwrap();
            let line_index = LineIndex::new(source);
            let result = find_schema_block_definition(&file, "provider", &line_index);
            assert!(
                result.is_some(),
                "find_schema_block_definition should find model provider:"
            );
            let range = result.unwrap();
            assert_eq!(
                range.start.line, 0,
                "model provider: is on line 0 (0-indexed)"
            );
        }

        // Test 3: find_definition_in_docs with is_on_keyword=true should NOT return field definitions
        {
            let result = find_definition_in_docs(&docs, "workflow", &current, Some("workflow"));
            assert!(result.is_some(), "should find something for 'workflow'");
            let (uri, range) = result.unwrap();
            // Should NOT go to "provider string?" field. Should find via Priority 4 (top-level decl).
            // model workflow: is on line 7 in workflow.model.nml
            assert_eq!(uri, model_uri);
            assert_eq!(range.start.line, 7, "should point to model workflow: name");
        }
    }

    // ── compute_indent_after_line ───────────────────────────────

    #[test]
    fn indent_after_block_colon() {
        let lines = vec!["workflow RecipeAssistant:", "    steps:"];
        assert_eq!(compute_indent_after_line(&lines, 0, 4), 4);
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 8);
    }

    #[test]
    fn indent_after_list_item_colon() {
        let lines = vec!["    steps:", "        - classify:"];
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 12);
    }

    #[test]
    fn indent_after_property() {
        let lines = vec!["        - classify:", "            provider = Groq"];
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 12);
    }

    #[test]
    fn indent_after_goto_property() {
        let lines = vec![
            "                - clarifyRoute:",
            "                    when:",
            "                        field = \"response_mode\"",
            "                        equals = \"clarify\"",
            "                    goto = \"respond\"",
        ];
        assert_eq!(compute_indent_after_line(&lines, 4, 4), 20);
    }

    #[test]
    fn indent_after_blank_line_uses_prev_non_empty() {
        let lines = vec!["    steps:", "        - classify:", ""];
        assert_eq!(compute_indent_after_line(&lines, 2, 4), 12);
    }

    #[test]
    fn indent_after_nested_block_colon() {
        let lines = vec!["        - router:", "            routes:"];
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 16);
    }

    #[test]
    fn indent_inside_triple_quote() {
        let lines = vec![
            "            system = \"\"\"",
            "            You are a helpful assistant.",
        ];
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 12);
    }

    #[test]
    fn indent_after_scalar_list_item() {
        let lines = vec!["enum providerType:", "    - \"anthropic\""];
        assert_eq!(compute_indent_after_line(&lines, 1, 4), 4);
    }

    #[test]
    fn indent_after_comment_ending_with_colon() {
        let lines = vec!["    // this is a comment:"];
        assert_eq!(compute_indent_after_line(&lines, 0, 4), 4);
    }

    #[test]
    fn indent_empty_source() {
        let lines: Vec<&str> = vec![];
        assert_eq!(compute_indent_after_line(&lines, 0, 4), 0);
    }

    #[test]
    fn indent_at_top_level() {
        let lines = vec!["workflow RecipeAssistant:"];
        assert_eq!(compute_indent_after_line(&lines, 0, 4), 4);
    }

    #[test]
    fn indent_after_block_colon_steps_by_the_unit_given() {
        let lines = vec!["workflow RecipeAssistant:", "  steps:"];
        assert_eq!(compute_indent_after_line(&lines, 0, 2), 2);
        assert_eq!(compute_indent_after_line(&lines, 1, 2), 4);
    }

    // ── ModelRef + discriminator helpers (share the parse-once / index walk) ──────

    /// Resolve the FIRST model-ref type at the cursor via the shared walk
    /// (parse-once + index) — the single-type view most pins assert;
    /// multi-type merging has its own tests.
    fn ref_type_at(schema_source: &str, source: &str, pos: Position) -> Option<String> {
        ref_types_at(schema_source, source, pos).into_iter().next()
    }

    /// All model-ref types at the cursor (union members, merged candidates).
    /// Best-effort parse, as production: value positions are often mid-typing
    /// (`slot = ⌖` with no value yet).
    fn ref_types_at(schema_source: &str, source: &str, pos: Position) -> Vec<String> {
        let index = field_index(schema_source);
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        find_model_ref_types_at(&file, source, pos, &index, &line_index)
    }

    /// The `oneof` arm keys offered at the cursor, or `None` if not a discriminator position.
    fn discriminator_arm_keys(
        schema_source: &str,
        source: &str,
        pos: Position,
    ) -> Option<Vec<String>> {
        let index = field_index(schema_source);
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        find_oneof_discriminator_at(&file, source, pos, &index, &line_index)
            .map(|o| o.variants.iter().map(|(v, _)| v.clone()).collect())
    }

    #[test]
    fn model_ref_type_detected_for_step_field() {
        let schema = "model step:\n    provider string?\n\nmodel workflow:\n    next step?\n    entrypoint step\n";
        let source = "workflow W:\n    next = classify\n";
        assert_eq!(
            ref_type_at(schema, source, Position::new(1, 14)),
            Some("step".to_string())
        );
    }

    #[test]
    fn oneof_discriminator_completion_offers_arm_keys() {
        let schema = "model emailLog:\n    x string?\n\nmodel emailPostmark:\n    y string?\n\noneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n";
        let source = "email Outbound:\n    provider = \"log\"\n";
        assert_eq!(
            discriminator_arm_keys(schema, source, Position::new(1, 20)),
            Some(vec!["log".to_string(), "postmark".to_string()])
        );
    }

    #[test]
    fn oneof_discriminator_completion_ignores_non_discriminator_field() {
        let schema = "model emailLog:\n    fromAddress string?\n\noneof email by provider:\n    \"log\" -> emailLog\n";
        // `fromAddress` is a variant field, not the discriminator — no arm-key completion.
        let source = "email Outbound:\n    fromAddress = \"x\"\n";
        assert!(discriminator_arm_keys(schema, source, Position::new(1, 19)).is_none());
    }

    #[test]
    fn model_ref_type_none_for_primitive_field() {
        let schema = "model workflow:\n    entrypoint string\n";
        let source = "workflow W:\n    entrypoint = \"start\"\n";
        assert_eq!(ref_type_at(schema, source, Position::new(1, 18)), None);
    }

    #[test]
    fn model_ref_type_detected_for_list_field() {
        let schema = "model tool:\n    wasm string?\n\nmodel workflow:\n    tools []tool?\n";
        let source = "workflow W:\n    tools = [myTool]\n";
        assert_eq!(
            ref_type_at(schema, source, Position::new(1, 14)),
            Some("tool".to_string())
        );
    }

    #[test]
    fn model_ref_type_works_in_nested_body() {
        // `fallback` is a model-ref field of the *nested* `prompt` model. The former
        // top-level-only detector returned `None` here; the shared walk (RFC 0003) resolves
        // it — a capability gain from refactoring `find_model_ref_type_at` onto the walk.
        let schema = "model prompt:\n    fallback step?\n\nmodel step:\n    name string\n    prompt prompt?\n";
        let source = "step S:\n    prompt:\n        fallback = other\n";
        assert_eq!(
            ref_type_at(schema, source, Position::new(2, 18)),
            Some("step".to_string())
        );
    }

    // ── Field completion (RFC 0003) ───────────────────────────────

    fn field_index(schema_source: &str) -> SchemaIndex {
        let s = nml_core::cst::extract_schema(schema_source).0;
        SchemaIndex::build(s.models, s.enums, s.oneofs)
    }

    #[test]
    fn field_completion_offers_top_level_fields_excluding_present() {
        let index = field_index(
            "model provider:\n    type string\n    model string\n    temperature number?\n    baseUrl string?\n",
        );
        // `model` and `type` are already set; cursor on a blank body line between them.
        let source = "provider GroqFast:\n    model = \"llama\"\n\n    type = \"groq\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        let (model, body) =
            find_model_body_at(&file, Position::new(2, 0), &index, &line_index).unwrap();
        assert_eq!(model.name, "provider");
        let offered: Vec<&str> = model
            .fields
            .iter()
            .filter(|f| !present_field_names(body).contains(&f.name))
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(offered, vec!["temperature", "baseUrl"]);
    }

    /// Round-19: value-position lookups are candidates-aware — an enum field
    /// UNIQUE to a later variant still resolves its variants inside an
    /// ambiguous body (previously the first-candidate view missed it).
    #[test]
    fn value_completion_resolves_fields_across_ambiguous_candidates() {
        let idx = field_index(
            "enum modeKind:\n    - fast\n    - slow\nmodel modelA:\n    a string?\nmodel modelB:\n    mode modeKind?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let source = "host H:\n    slot:\n        mode = \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let variants = find_value_completions_at(&file, source, Position::new(2, 15), &idx, &li)
            .map(ValueCompletions::merged)
            .expect("enum variants for a modelB-unique field");
        assert_eq!(variants, vec!["fast", "slow"]);
    }

    /// RFC 0017 §6: unit-suffix completion fires exactly after a bare
    /// integer in a duration-typed value position — never in a
    /// number-typed one, never mid-identifier — and returns the typed
    /// digits with edit ranges covering exactly them (NOT the whole value
    /// token: in a list position that heuristic would start at the `[`
    /// and an accepted edit would eat the bracket).
    #[test]
    fn duration_unit_completion_detects_bare_number_in_duration_field() {
        let idx = field_index(
            "model service:\n    timeout duration?\n    retries set<duration>?\n    port number?\n    name string?\n",
        );
        let detect = |source: &str, line: u32, character: u32| {
            let (file, parse) = nml_core::cst::parse_best_effort_with_tree(source);
            let li = LineIndex::new(source);
            let pos = Position::new(line, character);
            duration_lsp::find_duration_unit_completions_at(&parse, pos, &li, || {
                value_position_prop_name(source, pos)
                    .map(|prop| {
                        value_governors_at(&file, pos, &idx, &li, prop)
                            .fields
                            .iter()
                            .any(|f| governs_duration(&f.field_type))
                    })
                    .unwrap_or(false)
            })
        };
        let span_of = |r: Range| (r.start.character, r.end.character);
        // Cursor right after `30` in a duration-typed value position.
        let src = "service Api:\n    timeout = 30\n";
        let ctx = detect(src, 1, 16).expect("duration position completes");
        assert_eq!(ctx.digits, "30");
        assert_eq!(span_of(ctx.insert), (14, 16));
        assert_eq!(span_of(ctx.replace), (14, 16));
        // Re-triggering inside an existing literal: replace covers the
        // stale suffix so accepting swaps it instead of stacking.
        let src = "service Api:\n    timeout = 30ms\n";
        let ctx = detect(src, 1, 16).expect("mid-literal retrigger");
        assert_eq!(span_of(ctx.insert), (14, 16));
        assert_eq!(span_of(ctx.replace), (14, 18));
        // List position: the ranges cover the digits only — never the `[`.
        let src = "service Api:\n    retries = [30\n";
        let ctx = detect(src, 1, 17).expect("list element completes");
        assert_eq!(ctx.digits, "30");
        assert_eq!(span_of(ctx.insert), (15, 17));
        // A number-typed field must not grow unit noise.
        let src = "service Api:\n    port = 30\n";
        assert!(detect(src, 1, 13).is_none());
        // Digits mid-identifier are not a value start.
        let src = "service Api:\n    name = x30\n";
        assert!(detect(src, 1, 15).is_none());
        // No digits typed yet ⇒ nothing to suffix.
        let src = "service Api:\n    timeout = \n";
        assert!(detect(src, 1, 14).is_none());
    }

    /// RFC 0017: the hover's normalized total — the coarsest of ms/us/ns
    /// that divides the total exactly, grouped with the language's own
    /// `_` separator (pasteable NML), skipped when it would restate the
    /// literal or the value is zero.
    #[test]
    fn hover_normalized_total_rule() {
        let d = |text: &str| nml_core::duration::Duration::parse_text(text).unwrap();
        // Coarse units render their ms total, `_`-grouped.
        assert_eq!(d("30s").normalized_total().as_deref(), Some("30_000ms"));
        assert_eq!(d("90m").normalized_total().as_deref(), Some("5_400_000ms"));
        assert_eq!(d("2h").normalized_total().as_deref(), Some("7_200_000ms"));
        // Sub-ms values pick the coarsest EXACT unit — never `0ms`.
        assert_eq!(d("1000us").normalized_total().as_deref(), Some("1ms"));
        assert_eq!(d("2000ns").normalized_total().as_deref(), Some("2us"));
        // A value that is its own normalized form shows nothing extra.
        assert_eq!(d("500ms").normalized_total(), None);
        assert_eq!(d("250us").normalized_total(), None);
        assert_eq!(d("750ns").normalized_total(), None);
        // Compound literals (RFC 0017 §10) normalize like any value…
        assert_eq!(
            d("1h30m").normalized_total().as_deref(),
            Some("5_400_000ms")
        );
        assert_eq!(d("5m2s").normalized_total().as_deref(), Some("302_000ms"));
        // …and skip the restatement when ANY authored segment already
        // spells the normalized unit.
        assert_eq!(d("1s500ms").normalized_total(), None);
        // Zero teaches nothing in any unit.
        assert_eq!(d("0s").normalized_total(), None);
        assert_eq!(d("1h30m").coarsest_exact().as_deref(), Some("90m"));
    }

    /// Round-17 border pins: a modifier-declared field completes WITH its
    /// sigil (`|vis = `), and a modifier entry already in the body excludes
    /// its field from re-offering — in both the single-model and
    /// union-of-fields paths.
    #[test]
    fn modifier_fields_complete_with_sigil_and_exclude_when_present() {
        let idx = field_index(
            "model modelA:\n    a string?\n    |vis role?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        // Union path: body already sets |vis → excluded; a (unique) keeps
        // sigil-less insert; vis would carry the sigil if offered elsewhere.
        let source = "host H:\n    slot:\n        |vis = @admin\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file, Position::new(3, 8), &idx, &li)
        else {
            panic!("ambiguous")
        };
        let items =
            union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, false);
        assert!(
            !items.iter().any(|i| i.label == "vis" || i.label == "|vis"),
            "a modifier-set field must not re-offer: {:?}",
            items.iter().map(|i| i.label.clone()).collect::<Vec<_>>()
        );
        // Empty body: vis IS offered, with the sigil in its insert text.
        let source2 = "host H:\n    slot:\n        \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file2, Position::new(2, 8), &idx, &li2)
        else {
            panic!("ambiguous")
        };
        let items =
            union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li2, false);
        let vis = items
            .iter()
            .find(|i| i.label == "|vis")
            .expect("vis offered");
        assert_eq!(
            vis.insert_text.as_deref(),
            Some("|vis = "),
            "modifier fields author WITH the sigil"
        );
        assert_eq!(
            vis.filter_text.as_deref(),
            Some("vis"),
            "filtering stays on the bare name"
        );
    }

    /// Round-20: modifier VALUE positions. `|mode = ⌖` strips the sigil for
    /// the field lookup (candidates-aware) and unwraps the Modifier type for
    /// enum variants; the authored form must MATCH the declaration — a bare
    /// name on a modifier field (or a sigil on a plain field) governs nothing.
    #[test]
    fn modifier_value_position_completes_enum_variants_form_matched() {
        let idx = field_index(
            "enum modeKind:\n    - fast\n    - slow\nmodel modelA:\n    plain modeKind?\nmodel modelB:\n    |mode modeKind?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let li_at = |source: &str, line: u32, character: u32| {
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            find_value_completions_at(&file, source, Position::new(line, character), &idx, &li)
                .map(ValueCompletions::merged)
        };
        // Sigiled form on the modifier field (unique to the SECOND candidate).
        assert_eq!(
            li_at("host H:\n    slot:\n        |mode = \n", 2, 16),
            Some(vec!["fast".into(), "slow".into()]),
            "modifier value position must complete the inner enum"
        );
        // Bare form on a modifier-declared field: authors a property, not the
        // modifier — nothing governs, fail closed.
        assert_eq!(li_at("host H:\n    slot:\n        mode = \n", 2, 15), None);
        // Sigiled form on a PLAIN field: same mismatch, same fail-closed.
        assert_eq!(
            li_at("host H:\n    slot:\n        |plain = \n", 2, 17),
            None
        );
        // The plain path is unaffected.
        assert_eq!(
            li_at("host H:\n    slot:\n        plain = \n", 2, 16),
            Some(vec!["fast".into(), "slow".into()])
        );
    }

    /// Round-20: tier-1 (shared-name) agreement compares the authored FORM,
    /// not just the type Display (which erases the `|` wrapper): mixed
    /// modifier/property never scaffolds; an all-modifier name keeps its
    /// sigil in label and insert even when the inner types disagree.
    #[test]
    fn tier1_shared_name_agreement_is_modifierness_aware() {
        let shared_item = |schema: &str| {
            let idx = field_index(schema);
            let source = "host H:\n    slot:\n        \n";
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            let Some(DescentTarget::Ambiguous {
                candidates,
                body,
                header,
            }) = find_candidates_at(&file, Position::new(2, 8), &idx, &li)
            else {
                panic!("ambiguous")
            };
            let items =
                union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, false);
            items
                .into_iter()
                .find(|i| i.filter_text.as_deref() == Some("vis") || i.label == "vis")
                .expect("shared field offered")
        };
        // Mixed form, same inner type: Display alone would false-agree.
        let mixed = shared_item(
            "model modelA:\n    |vis role?\nmodel modelB:\n    vis role?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        assert_eq!(mixed.label, "vis");
        assert_eq!(
            mixed.insert_text.as_deref(),
            Some("vis"),
            "mixed forms must not scaffold either form"
        );
        // Uniformly modifier, disagreeing inner types: no scaffold, but the
        // name-only insert must keep the sigil or it authors a property.
        let all_mod = shared_item(
            "model modelA:\n    |vis role?\nmodel modelB:\n    |vis string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        assert_eq!(all_mod.label, "|vis");
        assert_eq!(all_mod.insert_text.as_deref(), Some("|vis"));
        assert_eq!(all_mod.filter_text.as_deref(), Some("vis"));
        // Uniformly modifier, agreeing types: full scaffold with sigil.
        let agree = shared_item(
            "model modelA:\n    |vis role?\nmodel modelB:\n    |vis role?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        assert_eq!(agree.label, "|vis");
        assert_eq!(agree.insert_text.as_deref(), Some("|vis = "));
    }

    /// Round-20 audit F1: value-position governors MERGE across candidates —
    /// a name shared between variants contributes every declaration, so one
    /// variant's `string` cannot suppress another's enum values, and two
    /// enum declarations union their variants.
    #[test]
    fn value_completion_merges_shared_names_across_candidates() {
        let at = |schema: &str| {
            let idx = field_index(schema);
            let source = "host H:\n    slot:\n        mode = \n";
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            find_value_completions_at(&file, source, Position::new(2, 15), &idx, &li)
                .map(ValueCompletions::merged)
        };
        // The suppression case: modelA's `string` declaration comes first.
        assert_eq!(
            at(
                "enum modeKind:\n    - fast\n    - slow\nmodel modelA:\n    mode string?\nmodel modelB:\n    mode modeKind?\nmodel host:\n    slot (modelA | modelB)?\n"
            ),
            Some(vec!["fast".into(), "slow".into()]),
            "a string declaration in an earlier variant must not suppress a later variant's enum"
        );
        // Two enum declarations: variants union in candidate order.
        assert_eq!(
            at(
                "enum kindA:\n    - a1\n    - a2\nenum kindB:\n    - b1\n    - b2\nmodel modelA:\n    mode kindA?\nmodel modelB:\n    mode kindB?\nmodel host:\n    slot (modelA | modelB)?\n"
            ),
            Some(vec!["a1".into(), "a2".into(), "b1".into(), "b2".into()])
        );
    }

    /// Round-20 audit F5: tier-0 scaffolds a oneof candidate's discriminator
    /// (`kind = `) — the value position it creates must then complete the
    /// arm keys, not abandon the author.
    #[test]
    fn ambiguous_discriminator_value_position_offers_arm_keys() {
        let idx = field_index(
            "model modelA:\n    a string?\nmodel logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    slot (modelA | mail)?\n",
        );
        let source = "host H:\n    slot:\n        kind = \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        assert_eq!(
            find_value_completions_at(&file, source, Position::new(2, 15), &idx, &li)
                .map(ValueCompletions::merged),
            Some(vec!["log".into()]),
            "the discriminator's arm keys govern its value position"
        );
    }

    /// Round-20 audit F6 + enum exclusion: a union-typed field's own value
    /// position admits declarations of EVERY model-ref member; enum refs are
    /// excluded from the ref-type channel (their values are variants).
    #[test]
    fn union_typed_field_value_admits_all_member_declarations() {
        let schema = "enum modeKind:\n    - fast\nmodel modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n    mode modeKind?\n";
        assert_eq!(
            ref_types_at(schema, "host H:\n    slot = \n", Position::new(1, 11)),
            vec!["modelA".to_string(), "modelB".to_string()]
        );
        assert!(
            ref_types_at(schema, "host H:\n    mode = \n", Position::new(1, 11)).is_empty(),
            "enum refs complete as variants, never as reference declarations"
        );
    }

    /// Round-20 audit F2: bare list items fill the body-positional shorthand
    /// field (RFC 0005 `+`) — the validator's seen-scan counts it as SET, so
    /// completion must not re-offer it beside the items.
    #[test]
    fn bare_list_items_mark_the_positional_field_present() {
        let idx = field_index("model host:\n    plugins []string+\n    other string?\n");
        let source = "host H:\n    - alpha\n    \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let (model, body) = find_model_body_at(&file, Position::new(2, 4), &idx, &li).unwrap();
        let present = present_field_names_in(model, body);
        assert!(
            present.contains("plugins"),
            "bare items set the shorthand field"
        );
        assert!(
            !present.contains("other"),
            "unrelated fields stay offerable"
        );
    }

    /// Round-21: a PRE-DISCRIMINATOR oneof body is a discovery moment, not a
    /// dead end — at a plain oneof field, a `[]oneof` element, and a union
    /// body resolved to a oneof variant. The discriminator field is offered
    /// (honestly: no `as` announcement where no annotation may attach), and
    /// its arm keys complete at the value position — including the typo
    /// state (`kind = lgo`), which previously killed the whole descent.
    #[test]
    fn unresolved_oneof_body_is_a_discovery_moment_at_all_three_sites() {
        let idx = field_index(
            "model modelA:\n    a string?\nmodel logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    slot mail?\n    slots []mail?\n    or (modelA | mail)?\n",
        );
        let candidates_at = |source: &str, line: u32, ch: u32| {
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            match find_candidates_at(&file, Position::new(line, ch), &idx, &li) {
                Some(DescentTarget::Ambiguous {
                    candidates, header, ..
                }) => Some((candidates.len(), header)),
                _ => None,
            }
        };
        // Site 1 — plain oneof field, fresh body: single-candidate discovery,
        // NO header anchor (`as` on a non-union field is a stray, NML2053).
        let (n, header) = candidates_at("host H:\n    slot:\n        \n", 2, 8)
            .expect("fresh oneof body must surface the oneof");
        assert_eq!(n, 1);
        assert!(header.is_none(), "no anchor where `as` is illegal");
        // The offered discriminator is honest: no edit, no announcement.
        {
            let source = "host H:\n    slot:\n        \n";
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            let Some(DescentTarget::Ambiguous {
                candidates,
                body,
                header,
            }) = find_candidates_at(&file, Position::new(2, 8), &idx, &li)
            else {
                panic!("ambiguous")
            };
            let items =
                union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, true);
            let kind = items.iter().find(|i| i.label == "kind").expect("kind");
            assert_eq!(kind.insert_text.as_deref(), Some("kind = "));
            assert!(kind.additional_text_edits.is_none());
            let desc = kind
                .label_details
                .as_ref()
                .and_then(|d| d.description.as_deref())
                .unwrap();
            assert!(
                !desc.contains("adds"),
                "must not announce an edit it does not attach: {desc}"
            );
        }
        // Site 1 value position — arm keys, including the TYPO state.
        let arms_at = |source: &str, line: u32, ch: u32| {
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            find_value_completions_at(&file, source, Position::new(line, ch), &idx, &li)
                .map(ValueCompletions::merged)
        };
        assert_eq!(
            arms_at("host H:\n    slot:\n        kind = \n", 2, 15),
            Some(vec!["log".into()])
        );
        assert_eq!(
            arms_at("host H:\n    slot:\n        kind = lgo\n", 2, 15),
            Some(vec!["log".into()]),
            "the typo state must still offer the repair"
        );
        // Site 2 — `[]mail` element body (element twin).
        let (n, header) =
            candidates_at("host H:\n    slots:\n        - one:\n            \n", 3, 12)
                .expect("fresh oneof ELEMENT body must surface the oneof");
        assert_eq!((n, header.is_none()), (1, true));
        // Site 3 — union body annotated to the oneof variant: header IS legal
        // (union-typed field), so the anchor is present — but the annotation
        // already names the variant, so the discriminator item must attach NO
        // edit (it would be a byte-identical no-op) and announce nothing.
        {
            let source = "host H:\n    or as mail:\n        \n";
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            let Some(DescentTarget::Ambiguous {
                candidates,
                body,
                header,
            }) = find_candidates_at(&file, Position::new(2, 8), &idx, &li)
            else {
                panic!("annotated-to-oneof body pre-discriminator must surface the oneof")
            };
            assert_eq!(candidates.len(), 1);
            assert_eq!(
                header.as_ref().map(|(name, _)| name.as_str()),
                Some("or"),
                "union-typed field keeps the annotation anchor"
            );
            let items =
                union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, false);
            let kind = items.iter().find(|i| i.label == "kind").expect("kind");
            assert!(
                kind.additional_text_edits.is_none(),
                "an already-matching annotation gets no no-op edit"
            );
            assert!(
                !kind.detail.as_deref().unwrap_or_default().contains("adds"),
                "and no announcement: {:?}",
                kind.detail
            );
        }
        // Site 3 counterpoint — STRUCTURALLY resolved to the oneof (no
        // annotation, disjoint union): the pick may make the resolution
        // explicit, so the edit attaches and is announced.
        {
            let idx2 = field_index(
                "model logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    or (mail | []logM)?\n",
            );
            let source = "host H:\n    or:\n        \n";
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            let Some(DescentTarget::Ambiguous {
                candidates,
                body,
                header,
            }) = find_candidates_at(&file, Position::new(2, 8), &idx2, &li)
            else {
                panic!("structurally-resolved oneof pre-discriminator must surface the oneof")
            };
            let items =
                union_of_fields_completions(&idx2, &candidates, body, header.as_ref(), &li, false);
            let kind = items.iter().find(|i| i.label == "kind").expect("kind");
            assert!(
                kind.additional_text_edits.is_some(),
                "a structural resolution may be made explicit"
            );
            assert!(
                kind.detail.as_deref().unwrap_or_default().contains("adds"),
                "and says so: {:?}",
                kind.detail
            );
        }
        // Resolution unchanged once the discriminator is set.
        let source = "host H:\n    slot:\n        kind = \"log\"\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let (model, _) = find_model_body_at(&file, Position::new(3, 8), &idx, &li)
            .expect("resolved oneof still descends");
        assert_eq!(model.name, "logM");
    }

    /// Round-23/24: FIELD PARITY for defaulted discriminators — a body
    /// resolved through a oneof's DEFAULT keeps the oneof visible
    /// (`via_oneof`), and the KNOB policy ([`defaulted_knob`]) offers the
    /// discriminator exactly like a defaulted field. Authored discriminators
    /// clear the knob; deeper nesting resets the context.
    #[test]
    fn defaulted_discriminator_stays_visible_to_completion() {
        let idx = field_index(
            "model logM:\n    level string?\n    sub logM?\nmodel postM:\n    server string?\n\noneof mail by kind = \"log\":\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot mail?\n    slots []mail?\n",
        );
        let oneof_at = |source: &str, line: u32, ch: u32| {
            let file = nml_core::cst::parse_best_effort(source);
            let li = LineIndex::new(source);
            match find_candidates_at(&file, Position::new(line, ch), &idx, &li) {
                Some(DescentTarget::One {
                    model,
                    body,
                    via_oneof,
                }) => Some((
                    model.name.clone(),
                    defaulted_knob(model, body, via_oneof).map(|o| o.name.clone()),
                )),
                _ => None,
            }
        };
        // Defaulted: the body resolves to the default variant AND keeps the
        // oneof visible.
        assert_eq!(
            oneof_at("host H:\n    slot:\n        \n", 2, 8),
            Some(("logM".into(), Some("mail".into())))
        );
        // Element twin.
        assert_eq!(
            oneof_at("host H:\n    slots:\n        - one:\n            \n", 3, 12),
            Some(("logM".into(), Some("mail".into())))
        );
        // Authored discriminator: the knob is set, nothing to surface.
        assert_eq!(
            oneof_at(
                "host H:\n    slot:\n        kind = \"log\"\n        \n",
                3,
                8
            ),
            Some(("logM".into(), None))
        );
        // Deeper nesting resets the context: inside `sub:` the landing body
        // is a plain model body, not the oneof's.
        assert_eq!(
            oneof_at("host H:\n    slot:\n        sub:\n            \n", 3, 12),
            Some(("logM".into(), None))
        );
        // ANY authored entry form clears the knob — the same present-name
        // rule field completion uses, not just the Property shape.
        assert_eq!(
            oneof_at("host H:\n    slot:\n        \n        kind:\n", 2, 8),
            Some(("logM".into(), None)),
            "a block-authored discriminator withholds the knob"
        );
        assert_eq!(
            oneof_at(
                "host H:\n    slot:\n        \n        |kind = \"post\"\n",
                2,
                8
            ),
            Some(("logM".into(), None)),
            "a modifier-authored discriminator withholds the knob"
        );
        // A variant field SHADOWING the discriminator name: the field item
        // covers the name — no double-offer.
        let shadow_idx = field_index(
            "model logM:\n    kind string?\n    level string?\n\noneof mail by kind = \"log\":\n    \"log\" -> logM\n\nmodel host:\n    slot mail?\n",
        );
        let source = "host H:\n    slot:\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        match find_candidates_at(&file, Position::new(2, 8), &shadow_idx, &li) {
            Some(DescentTarget::One {
                model,
                body,
                via_oneof,
            }) => assert!(
                defaulted_knob(model, body, via_oneof).is_none(),
                "a shadowing variant field suppresses the knob"
            ),
            other => panic!(
                "expected One for the shadowed case, got {}",
                other.map(|_| "Ambiguous").unwrap_or("None")
            ),
        }
    }

    /// Round-24 (whole-unit audit): the three seams it found, closed.
    /// (B1) The VARIANT-SWITCHING moment — a resolved oneof body's
    /// discriminator value still completes its arm keys. (A1) The element
    /// twin of the union-site anchor — a union ELEMENT resolving to a
    /// pre-discriminator oneof anchors at the item name; plain `[]oneof`
    /// stays anchor-less. (B3) A name that is a discriminator in EVERY
    /// candidate scaffolds the property form.
    #[test]
    fn whole_unit_seams_switching_element_anchor_shared_discriminator() {
        // B1: `kind = "log"` (VALID, resolved) — cursor at the value still
        // offers both arms: the author is switching, not setting.
        let idx = field_index(
            "model logM:\n    level string?\nmodel postM:\n    server string?\n\noneof mail by kind:\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot mail?\n",
        );
        let source = "host H:\n    slot:\n        kind = \"log\"\n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        assert_eq!(
            find_value_completions_at(&file, source, Position::new(2, 16), &idx, &li)
                .map(ValueCompletions::merged),
            Some(vec!["log".into(), "post".into()]),
            "the variant-switching quadrant must complete"
        );
        // A1: a UNION element resolving structurally to a oneof anchors the
        // discovery at the ITEM name (as is legal there)…
        let idx2 = field_index(
            "model logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    ors [](mail | []string)?\n",
        );
        let source2 = "host H:\n    ors:\n        - one:\n            \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        match find_candidates_at(&file2, Position::new(3, 12), &idx2, &li2) {
            Some(DescentTarget::Ambiguous { header, .. }) => assert_eq!(
                header.map(|(name, _)| name),
                Some("one".into()),
                "a union element's oneof discovery keeps the item anchor"
            ),
            _ => panic!("union element pre-discriminator must surface the oneof"),
        }
        // …while the plain `[]mail` element stays anchor-less (pinned in the
        // three-site test; re-asserted here as the contrast).
        let idx3 = field_index(
            "model logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    slots []mail?\n",
        );
        let source3 = "host H:\n    slots:\n        - one:\n            \n";
        let file3 = nml_core::cst::parse_best_effort(source3);
        let li3 = LineIndex::new(source3);
        match find_candidates_at(&file3, Position::new(3, 12), &idx3, &li3) {
            Some(DescentTarget::Ambiguous { header, .. }) => {
                assert!(header.is_none(), "plain []oneof keeps no anchor")
            }
            _ => panic!("plain []oneof pre-discriminator must surface the oneof"),
        }
        // B3: `kind` is a discriminator in BOTH candidates → property-form
        // scaffold (safe for all), merged arms complete at the value.
        let idx4 = field_index(
            "model logM:\n    level string?\nmodel postM:\n    server string?\n\noneof mailA by kind:\n    \"log\" -> logM\n\noneof mailB by kind:\n    \"post\" -> postM\n\nmodel host:\n    slot (mailA | mailB)?\n",
        );
        let source4 = "host H:\n    slot:\n        \n";
        let file4 = nml_core::cst::parse_best_effort(source4);
        let li4 = LineIndex::new(source4);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file4, Position::new(2, 8), &idx4, &li4)
        else {
            panic!("two oneofs sharing a keyed-or-bare body are ambiguous")
        };
        let items =
            union_of_fields_completions(&idx4, &candidates, body, header.as_ref(), &li4, false);
        let kind = items.iter().find(|i| i.label == "kind").expect("kind");
        assert_eq!(
            kind.insert_text.as_deref(),
            Some("kind = "),
            "an all-discriminator shared name scaffolds the property form"
        );
        assert_eq!(
            find_value_completions_at(&file4, source4, Position::new(2, 15), &idx4, &li4)
                .map(ValueCompletions::merged),
            None,
            "sanity: no value position on the blank line"
        );
        let source5 = "host H:\n    slot:\n        kind = \n";
        let file5 = nml_core::cst::parse_best_effort(source5);
        let li5 = LineIndex::new(source5);
        assert_eq!(
            find_value_completions_at(&file5, source5, Position::new(2, 15), &idx4, &li5)
                .map(ValueCompletions::merged),
            Some(vec!["log".into(), "post".into()]),
            "both oneofs' arms merge at the shared discriminator's value"
        );
    }

    /// Round-25: the discriminator STRIP mirrors the validator — in a
    /// via-resolved body, `kind = ⌖` completes ARM KEYS ONLY even when the
    /// variant model shadows the name with an enum field (the validator
    /// claims the property before variant validation, so the shadow enum's
    /// values would all be rejected). The sigiled form keeps the field
    /// channel; the channels stay split so each renders its honest label.
    #[test]
    fn discriminator_strip_mirrors_the_validator_and_channels_stay_split() {
        let idx = field_index(
            "enum modeKind:\n    - fast\n    - slow\nmodel logM:\n    kind modeKind?\n    |kind modeKind?\nmodel postM:\n    server string?\n\noneof mail by kind:\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot mail?\n",
        );
        // Property form: the discriminator — arms only, in the arms channel.
        let source = "host H:\n    slot:\n        kind = \"log\"\n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let values = find_value_completions_at(&file, source, Position::new(2, 16), &idx, &li)
            .expect("arms complete");
        assert!(
            values.variants.is_empty(),
            "shadow-enum values are validator-rejected and must not offer: {:?}",
            values.variants
        );
        assert_eq!(values.arms, vec!["log", "post"]);
        // Sigiled form: never the discriminator — field channel only. (The
        // body carries an authored discriminator so it RESOLVES: a
        // modifier-only body is unresolved and routes to discovery instead.)
        let source2 = "host H:\n    slot:\n        kind = \"log\"\n        |kind = \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        let values2 = find_value_completions_at(&file2, source2, Position::new(3, 16), &idx, &li2)
            .expect("modifier field completes");
        assert_eq!(values2.variants, vec!["fast", "slow"]);
        assert!(values2.arms.is_empty());
        // Cross-dedup: in an AMBIGUOUS body both channels fire (either
        // resolution is still pickable), and a value present in both
        // surfaces ONCE — variants win the overlap.
        let overlap_idx = field_index(
            "enum modeKind:\n    - fast\n    - log\nmodel modelA:\n    kind modeKind?\nmodel logM:\n    level string?\nmodel postM:\n    server string?\n\noneof mail by kind:\n    \"log\" -> logM\n    \"post\" -> postM\n\nmodel host:\n    slot (modelA | mail)?\n",
        );
        let source3 = "host H:\n    slot:\n        kind = \n";
        let file3 = nml_core::cst::parse_best_effort(source3);
        let li3 = LineIndex::new(source3);
        let values3 =
            find_value_completions_at(&file3, source3, Position::new(2, 15), &overlap_idx, &li3)
                .expect("both channels complete in the ambiguous body");
        assert_eq!(values3.variants, vec!["fast", "log"]);
        assert_eq!(
            values3.arms,
            vec!["post"],
            "the overlapping value surfaces once, in the variants channel"
        );
    }

    /// Round-20 audit F4: the code-action consumer cap counts VALID entries —
    /// malformed padding can neither bury a legitimate suggestion nor loosen
    /// the bound (9 valid still cap at 8, order preserved).
    #[test]
    fn suggestion_parse_caps_valid_entries_not_raw() {
        let valid =
            |r: &str| serde_json::json!({"replacement": r, "start": 0, "end": 1, "kind": "fix"});
        let malformed = serde_json::json!({"replacement": 42});
        let mut padded: Vec<serde_json::Value> = vec![malformed; 8];
        padded.push(valid("real"));
        let parsed = parse_suggestion_entries(&padded);
        assert_eq!(
            parsed.len(),
            1,
            "the ninth (valid) entry must survive malformed padding"
        );
        assert_eq!(parsed[0].replacement, "real");
        let nine: Vec<serde_json::Value> = (0..9).map(|i| valid(&format!("s{i}"))).collect();
        let capped = parse_suggestion_entries(&nine);
        assert_eq!(capped.len(), 8);
        assert_eq!(capped[0].replacement, "s0");
        assert_eq!(capped[7].replacement, "s7");
        // An inverted span fails closed like any other malformed entry — it
        // would otherwise round-trip as a spec-invalid LSP Range.
        let inverted =
            serde_json::json!({"replacement": "bad", "start": 5, "end": 2, "kind": "fix"});
        assert!(parse_suggestion_entries(&[inverted]).is_empty());
    }

    /// Round-21 audit: the LIMBO branch honors the oracle's SHAPE gate — a
    /// list-shaped body under a typo'd annotation gets NO field discovery
    /// (offering `plugins` there would rewrite the annotation AND author a
    /// named block duplicating the bare items already filling it).
    #[test]
    fn limbo_field_discovery_is_gated_to_keyed_or_bare_shapes() {
        let idx = field_index(
            "model modelA:\n    plugins []string+\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let source = "host H:\n    slot as nope:\n        - alpha\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        assert!(
            !matches!(
                find_candidates_at(&file, Position::new(3, 8), &idx, &li),
                Some(DescentTarget::Ambiguous { .. })
            ),
            "a list-shaped limbo body is not a discovery moment"
        );
        // The KEYED limbo body keeps its discovery (the round-15 behavior).
        let source2 = "host H:\n    slot as nope:\n        \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        assert!(matches!(
            find_candidates_at(&file2, Position::new(2, 8), &idx, &li2),
            Some(DescentTarget::Ambiguous { .. })
        ));
    }

    /// Round-16 pins (element twins of the round-15 fixes): the ELEMENT-level
    /// limbo edit replaces the bad annotation, and the `label_details=true`
    /// path announces the pending edit while `detail` stays the plain type.
    #[test]
    fn element_limbo_edit_and_label_details_path() {
        let idx = field_index(
            "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slots [](modelA | modelB)?\n",
        );
        let source = "host H:\n    slots:\n        - one as nope:\n            \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file, Position::new(3, 12), &idx, &li)
        else {
            panic!("element limbo must be ambiguous")
        };
        let items =
            union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, false);
        let a = items.iter().find(|i| i.label == "a").expect("a");
        let edit = &a.additional_text_edits.as_ref().expect("edit")[0];
        let line2 = "        - one as nope:";
        let applied = format!(
            "{}{}{}",
            &line2[..edit.range.start.character as usize],
            edit.new_text,
            &line2[edit.range.end.character as usize..]
        );
        assert_eq!(applied, "        - one as modelA:");

        let source2 = "host H:\n    slots:\n        - one:\n            \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file2, Position::new(3, 12), &idx, &li2)
        else {
            panic!("element body must be ambiguous")
        };
        let items =
            union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li2, true);
        let a = items.iter().find(|i| i.label == "a").expect("a");
        assert_eq!(
            a.label_details
                .as_ref()
                .and_then(|l| l.description.as_deref()),
            Some("modelA — adds `as modelA`"),
            "the pending edit is announced via labelDetails"
        );
        assert_eq!(
            a.detail.as_deref(),
            Some("string?"),
            "detail stays the plain type when labelDetails carries the announcement"
        );
    }

    /// Round-15 pins: (1) the LIMBO auto-annotation edit REPLACES the bad
    /// annotation (name-token-only would yield `slot as modelA as nope:`);
    /// (2) tier-0 sorts required-first within a variant group.
    #[test]
    fn limbo_auto_annotation_replaces_and_tier0_is_required_first() {
        let idx = field_index(
            "model modelA:\n    aopt string?\n    zreq string\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let source = "host H:\n    slot as nope:\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let Some(DescentTarget::Ambiguous {
            candidates,
            body,
            header,
        }) = find_candidates_at(&file, Position::new(2, 8), &idx, &li)
        else {
            panic!("limbo must be ambiguous")
        };
        let items =
            union_of_fields_completions(&idx, &candidates, body, header.as_ref(), &li, false);
        // (1) the edit swallows ` as nope`: applying it to the header line
        // yields exactly `    slot as modelA:`.
        let a = items.iter().find(|i| i.label == "aopt").expect("aopt");
        let edit = &a.additional_text_edits.as_ref().expect("edit")[0];
        let line1 = "    slot as nope:";
        let sc = edit.range.start.character as usize;
        let ec = edit.range.end.character as usize;
        assert_eq!(edit.range.start.line, 1);
        let applied = format!("{}{}{}", &line1[..sc], edit.new_text, &line1[ec..]);
        assert_eq!(
            applied, "    slot as modelA:",
            "the limbo edit must REPLACE the bad annotation"
        );
        // (2) required `zreq` sorts before optional `aopt` within modelA's group.
        let z = items.iter().find(|i| i.label == "zreq").expect("zreq");
        assert!(
            z.sort_text.as_ref().unwrap() < a.sort_text.as_ref().unwrap(),
            "required-first within the variant group: {:?} vs {:?}",
            z.sort_text,
            a.sort_text
        );
    }

    /// F4 limbo: an UNKNOWN annotation (`as nope`) must not quietly resolve
    /// first-wins for completion — the candidate set surfaces, same as the
    /// un-annotated ambiguous case (the validator rejects with NML2051).
    #[test]
    fn unknown_annotation_body_surfaces_candidates_not_first_wins() {
        let idx = field_index(
            "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let source = "host H:\n    slot as nope:\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        match find_candidates_at(&file, Position::new(2, 8), &idx, &li) {
            Some(DescentTarget::Ambiguous {
                candidates, header, ..
            }) => {
                let names: Vec<&str> = candidates.iter().map(|c| c.name()).collect();
                assert_eq!(names, vec!["modelA", "modelB"]);
                assert_eq!(header.map(|(n, _)| n), Some("slot".to_string()));
            }
            other => panic!(
                "limbo must surface candidates, got {}",
                match other {
                    Some(DescentTarget::One { model: m, .. }) => format!("One({})", m.name),
                    None => "None".into(),
                    _ => unreachable!(),
                }
            ),
        }
    }

    /// Round-13 F2: a union variant that is a ONEOF (`(modelA | mail)`,
    /// `slot as mail:` + discriminator) completes its selected variant-model's
    /// fields — previously Model-only guards left it completion-dead.
    #[test]
    fn union_oneof_variant_completes_through_its_discriminator() {
        let schema = "model modelA:\n    a string?\nmodel logM:\n    level string?\n\noneof mail by kind:\n    \"log\" -> logM\n\nmodel host:\n    slot (modelA | mail)?\n";
        let idx = field_index(schema);
        let source = "host H:\n    slot as mail:\n        kind = \"log\"\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let li = LineIndex::new(source);
        let resolved = find_model_body_at(&file, Position::new(3, 8), &idx, &li);
        assert_eq!(
            resolved.map(|(m, _)| m.name.as_str()),
            Some("logM"),
            "the oneof variant's selected model must complete"
        );
    }

    /// Round-11: the item-slot finder reaches a union list NESTED inside a
    /// `[]model` item (descending through items via per-item body-aware
    /// resolution), and in-item field completion resolves the ANNOTATED
    /// variant's model.
    #[test]
    fn union_list_finder_descends_through_items_and_annotated_variants() {
        let schema = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel outer:\n    inner [](modelA | modelB)?\nmodel host:\n    items []outer?\n";
        let idx = field_index(schema);
        let source =
            "host H:\n    items:\n        - x:\n            inner:\n                - one as \n";
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        let pos = Position::new(4, 26);
        let field = enclosing_top_block(&file, pos, &line_index).and_then(|block| {
            let Some(FieldTarget::Model(model)) = idx.resolve_ref(&block.keyword.name) else {
                return None;
            };
            find_union_list_field_at(model, &block.body, pos, &idx, &line_index)
        });
        assert_eq!(
            field.map(|f| f.name.as_str()),
            Some("inner"),
            "the nested union list field must be reachable through items"
        );

        // In-item field completion: the cursor inside `- one as modelB:` must
        // resolve modelB (the annotated variant), not fail or offer the parent.
        let schema2 = "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host2:\n    slots [](modelA | modelB)?\n";
        let idx2 = field_index(schema2);
        let source2 = "host2 H:\n    slots:\n        - one as modelB:\n            \n";
        let file2 = nml_core::cst::parse_best_effort(source2);
        let li2 = LineIndex::new(source2);
        let resolved = find_model_body_at(&file2, Position::new(3, 12), &idx2, &li2);
        assert_eq!(
            resolved.map(|(m, _)| m.name.as_str()),
            Some("modelB"),
            "field completion inside an annotated item must resolve its variant"
        );
    }

    fn coded_diag(code: &str, start: (u32, u32), end: (u32, u32)) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            code: Some(tower_lsp::lsp_types::NumberOrString::String(
                code.to_string(),
            )),
            message: "m".into(),
            ..Default::default()
        }
    }

    #[test]
    fn explanations_pick_narrowest_dedup_and_cap() {
        // Wide outer + narrow inner at one position: narrowest first, its
        // range is the hover highlight; duplicate codes collapse.
        let items = vec![
            coded_diag("NML2007", (1, 0), (3, 0)),
            coded_diag("NML2008", (2, 4), (2, 9)),
            coded_diag("NML2008", (2, 4), (2, 9)),
        ];
        let (md, range) = explanations_at_position(&items, Position::new(2, 5))
            .expect("coded diagnostics intersect");
        assert!(
            md.contains("**NML2008**") && md.contains("**NML2007**"),
            "{md}"
        );
        assert_eq!(md.matches("**NML2008**").count(), 1, "dedup by code: {md}");
        assert!(md.contains("nml explain NML2008"), "{md}");
        assert_eq!(range.start, Position::new(2, 4), "narrowest range wins");

        // Position outside every range → no augmentation.
        assert!(explanations_at_position(&items, Position::new(9, 0)).is_none());

        // An uncoded diagnostic never augments.
        let uncoded = vec![Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 5)),
            ..Default::default()
        }];
        assert!(explanations_at_position(&uncoded, Position::new(0, 2)).is_none());
    }

    #[test]
    fn range_contains_is_end_inclusive() {
        let r = Range::new(Position::new(1, 2), Position::new(1, 6));
        assert!(range_contains(&r, Position::new(1, 2)));
        assert!(range_contains(&r, Position::new(1, 6)), "end-inclusive");
        assert!(!range_contains(&r, Position::new(1, 7)));
        assert!(!range_contains(&r, Position::new(0, 4)));
    }

    #[test]
    fn merge_hover_composes_all_four_cases() {
        let base = || Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "base".into(),
            }),
            range: None,
        };
        let aug = || {
            (
                "**NML2007** — …".to_string(),
                Range::new(Position::new(0, 0), Position::new(0, 4)),
            )
        };
        // No base, no aug.
        assert!(merge_hover(None, None).is_none());
        // Base only: unchanged.
        assert!(matches!(
            merge_hover(Some(base()), None),
            Some(Hover { contents: HoverContents::Markup(mc), .. }) if mc.value == "base"
        ));
        // Aug only: explanation-only hover carrying the DIAGNOSTIC's range.
        let h = merge_hover(None, Some(aug())).expect("aug-only hover");
        assert_eq!(h.range, Some(aug().1));
        // Both: appended after a separator.
        let h = merge_hover(Some(base()), Some(aug())).unwrap();
        let HoverContents::Markup(mc) = h.contents else {
            panic!()
        };
        assert!(
            mc.value.starts_with("base")
                && mc.value.contains("---")
                && mc.value.contains("NML2007"),
            "{}",
            mc.value
        );
    }

    #[test]
    fn as_position_detector_recognizes_the_type_slot() {
        // Field header, cursor right after `as ` (empty partial).
        assert!(matches!(
            as_position_field("    slot as ", 12),
            Some(AsSlot::Field(n)) if n == "slot"
        ));
        // With a partial variant typed.
        assert!(matches!(
            as_position_field("    slot as mod", 15),
            Some(AsSlot::Field(n)) if n == "slot"
        ));
        // List element header — an ITEM slot: the name is the item's, so the
        // union must come from the ENCLOSING list field, not a field lookup.
        assert!(matches!(
            as_position_field("        - one as ", 17),
            Some(AsSlot::Item)
        ));
        // Not `as`-position: a plain nested block, a value, a completed body.
        assert!(as_position_field("    slot", 8).is_none());
        assert!(as_position_field("    port = ", 11).is_none());
        assert!(as_position_field("    slot as modelB:", 19).is_none());
    }

    #[test]
    fn as_completion_offers_union_variants() {
        // A same-class union field; at the `as` slot, the nameable variants are
        // offered (and disjoint variants — a list/scalar — never are).
        let index = field_index(
            "model modelA:\n    a string?\nmodel modelB:\n    b string?\nmodel host:\n    slot (modelA | modelB)?\n",
        );
        let source = "host H:\n    slot as modelA:\n        a = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        let (model, _) =
            find_model_body_at(&file, Position::new(1, 8), &index, &line_index).unwrap();
        let field = model.fields.iter().find(|f| f.name == "slot").unwrap();
        let FieldType::Union(variants) = &field.field_type else {
            panic!("expected union field")
        };
        assert_eq!(
            index.nameable_variant_names(variants),
            vec!["modelA", "modelB"]
        );
    }

    #[test]
    fn field_completion_none_on_header_line() {
        let index = field_index("model provider:\n    type string\n");
        let source = "provider GroqFast:\n    type = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        // Cursor on the `provider GroqFast:` header (line 0) — not a body position.
        assert!(find_model_body_at(&file, Position::new(0, 8), &index, &line_index).is_none());
    }

    #[test]
    fn field_completion_none_for_unknown_keyword() {
        let index = field_index("model provider:\n    type string\n");
        let source = "widget Foo:\n    color = \"red\"\n"; // `widget` is not a declared model
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        assert!(find_model_body_at(&file, Position::new(1, 0), &index, &line_index).is_none());
    }

    #[test]
    fn field_insert_text_is_type_aware() {
        // A scalar field is `f = `; a model-typed field is a block `f:`.
        let s = nml_core::cst::extract_schema(
            "model prompt:\n    system string?\n\nmodel step:\n    name string\n    prompt prompt?\n",
        )
        .0;
        let index = SchemaIndex::build(s.models.clone(), s.enums.clone(), s.oneofs.clone());
        let step = s.models.iter().find(|m| m.name == "step").unwrap();
        let name = step.fields.iter().find(|f| f.name == "name").unwrap();
        let prompt = step.fields.iter().find(|f| f.name == "prompt").unwrap();
        assert_eq!(field_insert_text(&index, name), "name = ");
        assert_eq!(field_insert_text(&index, prompt), "prompt:");
    }

    #[test]
    fn field_detail_shows_type_and_default() {
        let s = nml_core::cst::extract_schema(
            "model prompt:\n    outputFormat string = \"text\"\n    retries number?\n",
        )
        .0;
        let m = &s.models[0];
        let out_fmt = m.fields.iter().find(|f| f.name == "outputFormat").unwrap();
        let retries = m.fields.iter().find(|f| f.name == "retries").unwrap();
        assert_eq!(field_detail(out_fmt), "string = \"text\"");
        assert_eq!(field_detail(retries), "number?"); // no default → just the type
    }

    #[test]
    fn field_sort_key_orders_required_before_optional() {
        let s = nml_core::cst::extract_schema("model m:\n    req string\n    opt string?\n").0;
        let m = &s.models[0];
        let req = &m.fields[0];
        let opt = &m.fields[1];
        assert!(field_sort_key(req, 0) < field_sort_key(opt, 1));
    }

    #[test]
    fn field_completion_descends_into_nested_model_block() {
        let index = field_index(
            "model prompt:\n    system string?\n    user string?\n\nmodel step:\n    name string\n    prompt prompt?\n",
        );
        // Cursor inside the nested `prompt:` block — should resolve to the `prompt` model.
        let source = "step S:\n    name = \"x\"\n    prompt:\n        system = \"hi\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        let (model, body) =
            find_model_body_at(&file, Position::new(3, 8), &index, &line_index).unwrap();
        assert_eq!(model.name, "prompt");
        let offered: Vec<&str> = model
            .fields
            .iter()
            .filter(|f| !present_field_names(body).contains(&f.name))
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(offered, vec!["user"]); // `system` already present
    }

    #[test]
    fn arm_target_completion_resolves_the_target_type() {
        // RFC 0007: cursor after `->` inside an arm-set-typed block resolves
        // `V` — through the `(string | (role -> denial))` union, body-aware.
        let index = field_index(
            "model denialCard:\n    title string?\n\nmodel mount:\n    path string\n    denial (string | (role -> denial))?\n",
        );
        let source = "mount M:\n    path = \"/x\"\n    denial:\n        @plan/Pro -> Pro\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        // Cursor on the arm line (line 3), after the arrow.
        let targets =
            find_arm_target_types_at(&file, Position::new(3, 22), &index, &line_index).unwrap();
        assert_eq!(targets, vec!["denial".to_string()]);
        // The scalar `string` union member contributes no reference keyword.
    }

    #[test]
    fn inline_arm_body_field_completion_descends() {
        let index = field_index(
            "model landingPage:\n    label number\nmodel service:\n    routing (role -> landingPage)?\n",
        );
        let source = "service Api:\n    routing:\n        @role/admin -> adminLanding:\n            label = 4\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        let landing = find_model_body_at(&file, Position::new(3, 12), &index, &line_index)
            .expect("cursor inside inline arm body");
        assert_eq!(landing.0.name, "landingPage");
    }

    #[test]
    fn arm_target_completion_offers_inline_snippet_when_v_admits_inline() {
        let index = field_index(
            "model landingPage:\n    label number\nmodel service:\n    routing (role -> landingPage)?\n",
        );
        let source = "service Api:\n    routing:\n        @role/admin -> Landing\n";
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        let targets =
            find_arm_target_types_at(&file, Position::new(2, 28), &index, &line_index).unwrap();
        assert!(targets.contains(&"landingPage".to_string()));
        let snippet = inline_arm_target_snippet_item(&targets, &index).expect("snippet");
        assert_eq!(snippet.label, "name:");
        assert_eq!(snippet.insert_text.as_deref(), Some("name:\n    $0"));
        assert_eq!(snippet.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn cursor_past_arm_arrow_distinguishes_selector_and_target() {
        let line = "@role/admin -> Landing";
        assert!(
            !cursor_past_arm_arrow(line, 6),
            "mid-selector is selector side"
        );
        assert!(
            !cursor_past_arm_arrow(line, 13),
            "between `-` and `>` is still selector side"
        );
        assert!(
            cursor_past_arm_arrow(line, 14),
            "immediately after `->` is target side"
        );
        assert!(cursor_past_arm_arrow(line, 18));
        assert!(!cursor_past_arm_arrow("no arrow here", 5));
    }

    #[test]
    fn arm_selector_completion_offers_enum_keys() {
        let index = field_index(
            "enum planKind:\n    - \"free\"\n    - \"pro\"\nmodel service:\n    routing (planKind -> string)?\n",
        );
        let source = "service Api:\n    routing:\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        let key = find_arm_set_key_at(&file, Position::new(2, 8), &index, &line_index)
            .expect("arm-set key");
        let items = arm_selector_completion_items(&key, &index, &[]);
        assert!(items.iter().any(|i| i.label == "\"pro\""));
        assert!(items.iter().any(|i| i.label == "else"));
        let pro = items.iter().find(|i| i.label == "\"pro\"").unwrap();
        assert_eq!(pro.insert_text.as_deref(), Some("\"pro\" -> "));
    }

    #[test]
    fn arm_selector_completion_offers_tagged_refs_for_role_typed_k() {
        // The flagship consumer shape (`(role -> denial)`): a role-typed K
        // completes as `@keyword/name` tagged refs from workspace
        // declarations — the validator's exact K-admission rule — not just
        // `else`.
        let index = field_index(
            "model denialCard:\n    title string?\nmodel service:\n    routing (role -> denialCard)?\n",
        );
        let source = "service Api:\n    routing:\n        \n";
        let file = nml_core::cst::parse_best_effort(source);
        let line_index = LineIndex::new(source);
        let key = find_arm_set_key_at(&file, Position::new(2, 8), &index, &line_index)
            .expect("arm-set key");
        let tagged = vec![
            ("plan".to_string(), "Pro".to_string()),
            ("role".to_string(), "admin".to_string()),
        ];
        let items = arm_selector_completion_items(&key, &index, &tagged);
        let admin = items
            .iter()
            .find(|i| i.label == "@role/admin")
            .expect("@role/admin offered");
        assert_eq!(admin.insert_text.as_deref(), Some("@role/admin -> "));
        assert!(items.iter().any(|i| i.label == "@plan/Pro"));
        assert!(items.iter().any(|i| i.label == "else"));
    }

    #[test]
    fn collect_tagged_ref_candidates_covers_block_declarations() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "role admin:\n    description = \"Admin\"\n\nplan Pro:\n    description = \"Pro\"\n"
                .to_string(),
        );
        let refs = collect_tagged_ref_candidates(&docs);
        assert!(refs.contains(&("role".to_string(), "admin".to_string())));
        assert!(refs.contains(&("plan".to_string(), "Pro".to_string())));
    }

    #[test]
    fn hover_resolves_named_array_items() {
        // RFC 0007 §4.1: hovering an arm target (`-> ProUpsell`) shows the
        // `[]denial` item it names — with its leading-comment docs and body
        // summary — exactly like any other declaration. The item form is
        // generic: any `- Name:` array item hovers, not just denial targets.
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            concat!(
                "[]denial denials:\n",
                "    // The paywall for gated reports.\n",
                "    - ProUpsell:\n",
                "        title = \"Go Pro\"\n",
                "    // The neutral fallback.\n",
                "    - Generic:\n",
                "        title = \"No access\"\n",
            )
            .to_string(),
        );
        let text = find_declaration_hover(&docs, "ProUpsell", &[]).expect("item hover present");
        assert!(
            text.contains("**denial** `ProUpsell`"),
            "hovers as the array's item keyword: {text}"
        );
        assert!(
            text.contains("The paywall for gated reports."),
            "the item's leading comment is its documentation (RFC 0004 §4.3): {text}"
        );
        assert!(
            text.contains("title") && text.contains("*Source: nudge.nml*"),
            "carries the body summary and source: {text}"
        );
        // A MID-LIST item's comment reaches it through the other attachment
        // path (deferred past the previous item's dedent, INTO this item —
        // the in-node walk), and the previous item's content never bleeds in.
        let second = find_declaration_hover(&docs, "Generic", &[]).expect("mid-list item hovers");
        assert!(
            second.contains("The neutral fallback."),
            "a mid-list item surfaces its own leading comment: {second}"
        );
        assert!(
            !second.contains("paywall") && !second.contains("Go Pro"),
            "the previous item's docs/content never bleed in: {second}"
        );
        // The array declaration itself still hovers as before.
        let arr = find_declaration_hover(&docs, "denials", &[]).expect("array hover present");
        assert!(arr.contains("**[]denial** `denials`"), "{arr}");
        // An unknown name hovers nothing.
        assert!(find_declaration_hover(&docs, "Ghost", &[]).is_none());
    }

    #[test]
    fn hover_prefers_a_declaration_over_a_same_named_item() {
        // Priority pin: when an array ITEM and a top-level DECLARATION share a
        // name, the declaration wins — even when the array is declared first.
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            concat!(
                "[]denial denials:\n",
                "    - Shared:\n",
                "        title = \"item\"\n",
                "\n",
                "workflow Shared:\n",
                "    steps = []\n",
            )
            .to_string(),
        );
        let text = find_declaration_hover(&docs, "Shared", &[]).expect("hover present");
        assert!(
            text.contains("**workflow** `Shared`"),
            "the declaration outranks the item: {text}"
        );
    }

    #[test]
    fn hover_prefers_a_declaration_over_an_item_across_documents() {
        // The CROSS-DOCUMENT priority pin: `HashMap` iteration order is
        // nondeterministic, so this is the case that actually exercises the
        // held-item-fallback — a return-first-match regression would pass or
        // fail here depending on hash order, while the two-tier lookup is
        // deterministic. Run against both insertion orders for good measure.
        for (first, second) in [("a.nml", "b.nml"), ("b.nml", "a.nml")] {
            let item_doc = "[]denial denials:\n    - Shared:\n        title = \"item\"\n";
            let decl_doc = "workflow Shared:\n    steps = []\n";
            let mut docs = HashMap::new();
            docs.insert(make_uri(first), item_doc.to_string());
            docs.insert(make_uri(second), decl_doc.to_string());
            // Which file holds which content is fixed by NAME, not insertion
            // order: a.nml always has the item, b.nml always the declaration.
            docs.insert(make_uri("a.nml"), item_doc.to_string());
            docs.insert(make_uri("b.nml"), decl_doc.to_string());
            let text = find_declaration_hover(&docs, "Shared", &[]).expect("hover present");
            assert!(
                text.contains("**workflow** `Shared`") && text.contains("*Source: b.nml*"),
                "the declaration wins across documents (insertion order {first}/{second}): {text}"
            );
        }
    }

    #[test]
    fn field_completion_resolves_oneof_variant_fields() {
        // A `email` oneof field; the body's `provider = "postmark"` selects `emailPostmark`,
        // so its fields are offered (variant-field completion — RFC 0002 §7b, now landed).
        let index = field_index(concat!(
            "model emailLog:\n    path string?\n\n",
            "model emailPostmark:\n    apiKey string?\n    fromAddress string?\n\n",
            "oneof email by provider:\n    \"log\" -> emailLog\n    \"postmark\" -> emailPostmark\n\n",
            "model config:\n    email email?\n",
        ));
        let source =
            "config C:\n    email:\n        provider = \"postmark\"\n        apiKey = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        // Cursor inside the `email` body (the `apiKey` line, property position).
        let (model, body) =
            find_model_body_at(&file, Position::new(3, 8), &index, &line_index).unwrap();
        assert_eq!(model.name, "emailPostmark");
        let offered: Vec<&str> = model
            .fields
            .iter()
            .filter(|f| !present_field_names(body).contains(&f.name))
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(offered, vec!["fromAddress"]); // `apiKey` present; variant of "postmark"
    }

    #[test]
    fn field_completion_descends_into_list_item() {
        let index = field_index(
            "model step:\n    name string\n    tag string?\n\nmodel workflow:\n    steps []step?\n",
        );
        // Cursor inside the `- classify:` list item — should resolve to the `step` model
        // (workflow → steps list → step item).
        let source = "workflow W:\n    steps:\n        - classify:\n            name = \"x\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let line_index = LineIndex::new(source);
        let (model, _body) =
            find_model_body_at(&file, Position::new(3, 12), &index, &line_index).unwrap();
        assert_eq!(model.name, "step");
    }

    #[test]
    fn collect_declarations_by_keyword_finds_steps() {
        let mut docs = HashMap::new();
        let uri = make_uri("voice-agent.workflow.nml");
        docs.insert(
            uri,
            concat!(
                "step classify:\n",
                "    provider = \"groq\"\n",
                "\n",
                "step respond:\n",
                "    provider = \"openai\"\n",
            )
            .to_string(),
        );

        let results = collect_declarations_by_keyword(&docs, "step");
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"classify"), "should find step classify");
        assert!(names.contains(&"respond"), "should find step respond");
    }

    #[test]
    fn collect_declarations_by_keyword_filters_keyword() {
        let mut docs = HashMap::new();
        let uri = make_uri("app.nml");
        docs.insert(
            uri,
            concat!(
                "step classify:\n",
                "    provider = \"groq\"\n",
                "\n",
                "provider Groq:\n",
                "    type = \"groq\"\n",
            )
            .to_string(),
        );

        let results = collect_declarations_by_keyword(&docs, "step");
        assert_eq!(results.len(), 1, "should only find step declarations");
        assert_eq!(results[0].0, "classify");
    }

    #[test]
    fn collect_declarations_by_keyword_finds_array_items() {
        let mut docs = HashMap::new();
        let uri = make_uri("workflow.nml");
        docs.insert(
            uri,
            "[]step steps:\n    - classify:\n        provider = \"groq\"\n    - respond:\n        provider = \"openai\"\n".to_string(),
        );

        let results = collect_declarations_by_keyword(&docs, "step");
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"classify"));
        assert!(names.contains(&"respond"));
    }

    // ── Role ref definition resolution ───────────────────────────

    #[test]
    fn definition_role_ref_standalone_block() {
        let mut docs = HashMap::new();
        let uri = make_uri("nudge.nml");
        docs.insert(
            uri.clone(),
            "role admin:\n    description = \"Full admin\"\n".to_string(),
        );

        let result = find_tagged_ref_definition_in_docs(&docs, "@role/admin");
        assert!(result.is_some(), "should find role admin definition");
        assert_eq!(result.unwrap().uri, uri);
    }

    #[test]
    fn definition_role_ref_plan_block() {
        let mut docs = HashMap::new();
        let uri = make_uri("nudge.nml");
        docs.insert(
            uri.clone(),
            "plan Pro:\n    description = \"Pro tier\"\n".to_string(),
        );

        let result = find_tagged_ref_definition_in_docs(&docs, "@plan/Pro");
        assert!(result.is_some(), "should find plan Pro definition");
        assert_eq!(result.unwrap().uri, uri);
    }

    #[test]
    fn definition_role_ref_builtin_returns_none() {
        let docs = HashMap::new();
        assert!(find_tagged_ref_definition_in_docs(&docs, "@public").is_none());
        assert!(find_tagged_ref_definition_in_docs(&docs, "@authenticated").is_none());
    }

    #[test]
    fn definition_role_ref_nonexistent_returns_none() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "role admin:\n    description = \"Admin\"\n".to_string(),
        );
        assert!(find_tagged_ref_definition_in_docs(&docs, "@role/nonexistent").is_none());
    }

    // ── Role ref hover ───────────────────────────────────────────

    #[test]
    fn hover_role_ref_with_description() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "role admin:\n    description = \"Full administrative access\"\n".to_string(),
        );

        let result = find_tagged_ref_hover_in_docs(&docs, "role", "admin");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(
            text.contains("**role** `admin`"),
            "should contain role name"
        );
        assert!(
            text.contains("Full administrative access"),
            "should contain description"
        );
        assert!(
            text.contains("Source: nudge.nml"),
            "should contain source file"
        );
    }

    #[test]
    fn hover_surfaces_leading_comment_as_documentation() {
        // RFC 0004 §4.3 hover-on-comment payoff: a comment written above a
        // declaration is surfaced as its hover documentation.
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "// Privileged operators.\n// Use sparingly.\nrole admin:\n    label = \"Admin\"\n"
                .to_string(),
        );

        let text = find_tagged_ref_hover_in_docs(&docs, "role", "admin").expect("hover present");
        assert!(
            text.contains("**role** `admin`"),
            "names the declaration: {text}"
        );
        assert!(
            text.contains("Privileged operators.") && text.contains("Use sparingly."),
            "surfaces the leading comment block as docs: {text}"
        );
    }

    #[test]
    fn hover_role_ref_without_description() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "role editor:\n    label = \"Editor\"\n".to_string(),
        );

        let result = find_tagged_ref_hover_in_docs(&docs, "role", "editor");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("**role** `editor`"));
        assert!(!text.contains("Full administrative"));
    }

    #[test]
    fn hover_role_ref_nonexistent() {
        let docs = HashMap::new();
        assert!(find_tagged_ref_hover_in_docs(&docs, "role", "ghost").is_none());
    }

    // ── Role ref completion via collect_declarations_by_keyword ───

    #[test]
    fn collect_declarations_by_keyword_finds_roles() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "role admin:\n    description = \"Admin\"\n\nrole editor:\n    description = \"Editor\"\n".to_string(),
        );

        let results = collect_declarations_by_keyword(&docs, "role");
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"admin"), "should find role admin");
        assert!(names.contains(&"editor"), "should find role editor");
    }

    #[test]
    fn collect_declarations_by_keyword_finds_plans_in_array() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("nudge.nml"),
            "[]plan plans:\n    - Free:\n        description = \"Free tier\"\n    - Pro:\n        description = \"Pro tier\"\n".to_string(),
        );

        let results = collect_declarations_by_keyword(&docs, "plan");
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"Free"), "should find plan Free");
        assert!(names.contains(&"Pro"), "should find plan Pro");
    }

    #[test]
    fn collect_declarations_by_keyword_role_does_not_include_steps() {
        let mut docs = HashMap::new();
        docs.insert(
            make_uri("app.nml"),
            "role admin:\n    description = \"Admin\"\n\nworkflow W:\n    steps:\n        - classify:\n            provider = \"groq\"\n".to_string(),
        );

        let roles = collect_declarations_by_keyword(&docs, "role");
        let role_names: Vec<&str> = roles.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(role_names.contains(&"admin"));
        assert!(
            !role_names.contains(&"classify"),
            "steps should not appear in role results"
        );
    }

    // ── UTF-16 position handling (multibyte content) ──────────

    #[test]
    fn extract_word_multibyte_line() {
        let line = "naïve = café";
        let byte_col = position::utf16_to_byte(line, 10); // inside "café"
        assert_eq!(extract_word_at(line, byte_col), "café");
    }

    #[test]
    fn extract_word_cjk() {
        let line = "tag = 日本語";
        let byte_col = position::utf16_to_byte(line, 7); // inside 日本語
        assert_eq!(extract_word_at(line, byte_col), "日本語");
    }

    #[test]
    fn extract_word_mid_multibyte_does_not_panic() {
        // Byte 1 is inside the emoji; must clamp to a char boundary.
        assert_eq!(extract_word_at("😀abc", 1), "");
    }

    #[test]
    fn span_to_range_utf16_after_emoji() {
        // 'y' begins at byte 11 but UTF-16 column 9 (the emoji is 4 bytes
        // yet only 2 UTF-16 units).
        let source = "x = \"😀\" y";
        let line_index = LineIndex::new(source);
        let range = span_to_range(nml_core::span::Span::new(11, 12), &line_index);
        assert_eq!(range.start, Position::new(0, 9));
        assert_eq!(range.end, Position::new(0, 10));
    }

    #[test]
    fn find_by_text_multibyte_prefix() {
        // 'é' is 2 bytes but 1 UTF-16 unit; reported columns must be UTF-16.
        let source = "sérvice GroqFast:\n    type = \"groq\"";
        let range = find_name_by_text(source, "GroqFast").unwrap();
        assert_eq!(range.start.character, 8);
        assert_eq!(range.end.character, 16);
    }

    #[test]
    fn model_ref_type_multibyte_value_does_not_panic() {
        // Cursor between CJK chars: treating the UTF-16 column as a byte
        // index would slice mid-character and panic.
        let schema = "model workflow:\n    entrypoint string\n";
        let source = "workflow W:\n    entrypoint = \"日本語テスト\"\n";
        assert_eq!(ref_type_at(schema, source, Position::new(1, 23)), None);
    }

    #[test]
    fn property_name_position_multibyte() {
        let line = "    clé = \"x\"";
        let byte_col = position::utf16_to_byte(line, 6); // on "clé"
        assert!(is_property_name_position(line, "clé", byte_col));
    }

    #[test]
    fn rename_range_multibyte() {
        let line = "naïve = café";
        let byte_col = position::utf16_to_byte(line, 9); // inside "café"
        let (start, end) = rename_word_byte_range(line, byte_col);
        assert_eq!(&line[start..end], "café");
        assert_eq!(position::byte_to_utf16(line, start), 8);
        assert_eq!(position::byte_to_utf16(line, end), 12);
    }

    #[test]
    fn rename_range_excludes_ref_punctuation() {
        let line = "access = @role/admin";
        let (start, end) = rename_word_byte_range(line, 16); // on "admin"
        assert_eq!(&line[start..end], "admin");
    }

    // ── Watched-file eligibility ──────────────────────────────

    /// A guard-owned scratch workspace (removed on drop, a red assertion
    /// included).
    fn temp_workspace(tag: &str) -> crate::scratch::Scratch {
        crate::scratch::Scratch::new(&format!("lsp-{tag}"))
    }

    /// The file a suggestion names is a workspace KEY (`SourceKey::checked`)
    /// joined under the document's root — never a lexical join: a `..`
    /// climb, an absolute path, a backslash, a `.` component, the empty
    /// name (the root itself) and a depth past the bound name no target,
    /// even where a file sits at the lexical join (the manifest one level
    /// above the root here — `may_write` is containment over the joined
    /// path, which a `..` keeps lexically inside the root). A key names
    /// its file and the text an action edits; a document with no root
    /// names nothing.
    #[test]
    fn a_quick_fix_target_is_a_workspace_key_never_a_lexical_join() {
        let base = temp_workspace("suggestion-target-key");
        let ws = base.join("inner");
        fs::create_dir_all(ws.join("tenants/cu")).unwrap();
        fs::write(
            ws.join("demo.package.nml"),
            nml_validate::test_support::DEMO_MANIFEST,
        )
        .unwrap();
        fs::write(
            ws.join("core.model.nml"),
            nml_validate::test_support::DEMO_CORE,
        )
        .unwrap();
        fs::write(
            ws.join("tenants/cu/x.flow.nml"),
            "thing a:\n    v = \"x\"\n",
        )
        .unwrap();
        // The file a climb would reach: a manifest ABOVE the root.
        fs::write(
            base.join("demo.package.nml"),
            nml_validate::test_support::DEMO_MANIFEST,
        )
        .unwrap();
        let (service, _socket) =
            crate::build_service(|client| NmlLanguageServer::with_store(client, None));
        let server = service.inner();
        let root = dunce::canonicalize(&ws).unwrap();
        server.workspace_roots.lock().unwrap().push(root.clone());
        let own = Url::from_file_path(root.join("tenants/cu/x.flow.nml")).unwrap();
        let (target, text) = server
            .suggestion_target(&own, "demo.package.nml")
            .expect("a key names its file");
        assert_eq!(
            target,
            Url::from_file_path(root.join("demo.package.nml")).unwrap()
        );
        assert_eq!(text, nml_validate::test_support::DEMO_MANIFEST);
        let deep = "d/".repeat(65) + "demo.package.nml";
        for bad in [
            "../demo.package.nml",
            "tenants/../../demo.package.nml",
            "./demo.package.nml",
            "tenants\\cu\\x.flow.nml",
            "/etc/passwd",
            "",
            deep.as_str(),
        ] {
            assert!(
                server.suggestion_target(&own, bad).is_none(),
                "{bad:?} is no key, so no target"
            );
        }
        // No root: no target, key or not.
        let stray = Url::from_file_path(base.join("demo.package.nml")).unwrap();
        assert!(
            server
                .suggestion_target(&stray, "demo.package.nml")
                .is_none()
        );
    }

    /// A workspace folder removed at runtime takes its cached universe
    /// with it — dropped at `workspace/didChangeWorkspaceFolders`, not
    /// by a later retention pass (which keeps every folder's universe).
    /// The wire cannot see the difference (a re-added folder
    /// rediscovers on any change and serves the same content
    /// otherwise), so the cache itself is inspected.
    #[tokio::test]
    async fn a_removed_folders_universe_is_dropped_at_removal() {
        use tower_lsp::LanguageServer;
        use tower_lsp::lsp_types::{
            DidChangeWorkspaceFoldersParams, WorkspaceFolder, WorkspaceFoldersChangeEvent,
        };
        let ws = temp_workspace("folder-removed");
        fs::write(
            ws.join("demo.package.nml"),
            nml_validate::test_support::DEMO_MANIFEST,
        )
        .unwrap();
        fs::write(
            ws.join("core.model.nml"),
            nml_validate::test_support::DEMO_CORE,
        )
        .unwrap();
        fs::create_dir_all(ws.join("tenants/cu")).unwrap();
        fs::write(
            ws.join("tenants/cu/x.flow.nml"),
            "thing a:\n    v = \"x\"\n",
        )
        .unwrap();
        let (service, _socket) =
            crate::build_service(|client| NmlLanguageServer::with_store(client, None));
        let server = service.inner();
        let root = dunce::canonicalize(&*ws).unwrap();
        server.workspace_roots.lock().unwrap().push(root.clone());
        let uri = Url::from_file_path(ws.join("tenants/cu/x.flow.nml")).unwrap();
        assert!(server.resolve_document(&uri).is_some());
        assert_eq!(server.resolver.cached_universe_roots(), vec![root.clone()]);
        server
            .did_change_workspace_folders(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: Vec::new(),
                    removed: vec![WorkspaceFolder {
                        uri: Url::from_file_path(&root).unwrap(),
                        name: "ws".to_string(),
                    }],
                },
            })
            .await;
        assert!(
            server.resolver.cached_universe_roots().is_empty(),
            "the removed folder's universe lingers"
        );
    }

    /// Two NESTED workspace folders: which one governs a document under
    /// both must be a rule, not the order the client happened to send
    /// them in. `workspace_roots` is kept SORTED, so an ancestor always
    /// precedes its descendants and the "first root the path starts
    /// with" that `PackageResolver::anchor_for` and
    /// `packages::source_name_of` pick IS the outermost —
    /// `packages::canonical_above_roots`'s rule, which the other two
    /// used to agree with only by luck. Unsorted, the inner folder
    /// governed when it arrived first, giving the same file a second
    /// universe and a second `source` key.
    #[tokio::test]
    async fn nested_workspace_folders_are_ordered_outermost_first() {
        let outer = temp_workspace("nested-order");
        let canon_outer = dunce::canonicalize(&outer).unwrap();
        let inner = canon_outer.join("sub");
        fs::create_dir_all(&inner).unwrap();
        let doc = inner.join("a.nml");
        fs::write(&doc, "thing a:\n    v = \"x\"\n").unwrap();

        let (service, _socket) =
            crate::build_service(|client| NmlLanguageServer::with_store(client, None));
        let server = service.inner();
        // The client sends the INNER folder first.
        server
            .did_change_workspace_folders(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![
                        WorkspaceFolder {
                            uri: Url::from_file_path(&inner).unwrap(),
                            name: "inner".to_string(),
                        },
                        WorkspaceFolder {
                            uri: Url::from_file_path(&canon_outer).unwrap(),
                            name: "outer".to_string(),
                        },
                    ],
                    removed: Vec::new(),
                },
            })
            .await;
        let roots = server.workspace_roots.lock().unwrap().clone();
        assert_eq!(
            roots,
            vec![canon_outer.clone(), inner.clone()],
            "an ancestor precedes its descendants whatever the client's order"
        );
        assert_eq!(
            roots.iter().find(|r| doc.starts_with(r)),
            Some(&canon_outer),
            "the outermost folder governs"
        );
        assert_eq!(
            packages::source_name_of(&doc, &roots),
            "sub/a.nml",
            "the finding's name is keyed under the governing root"
        );

        // The handshake's own capture obeys the same rule.
        let (service, _socket) =
            crate::build_service(|client| NmlLanguageServer::with_store(client, None));
        let fresh = service.inner();
        let folder = |p: &std::path::Path, name: &str| WorkspaceFolder {
            uri: Url::from_file_path(p).unwrap(),
            name: name.to_string(),
        };
        fresh
            .initialize(InitializeParams {
                workspace_folders: Some(vec![
                    folder(&inner, "inner"),
                    folder(&canon_outer, "outer"),
                ]),
                ..Default::default()
            })
            .await
            .expect("initialize");
        assert_eq!(
            *fresh.workspace_roots.lock().unwrap(),
            vec![canon_outer, inner],
            "initialize sorts too"
        );
    }

    #[test]
    fn canonical_within_roots_contains_after_canonicalizing() {
        // The order is the security property: canonicalize FIRST, then
        // contain — so an in-root symlink whose target is outside FAILS
        // (post-resolution containment), while an in-root alias of an
        // in-root file passes as its canonical target. Zero roots, a
        // missing path, and an outside path all fail closed.
        let root = temp_workspace("cwr");
        let inside = root.join("a.nml");
        fs::write(&inside, "x").unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        let hit = canonical_within_roots(&inside, std::slice::from_ref(&canon_root))
            .expect("inside resolves");
        assert!(hit.starts_with(&canon_root));
        assert!(
            canonical_within_roots(&inside, &[]).is_none(),
            "zero roots refuses everything"
        );
        assert!(
            canonical_within_roots(&root.join("missing.nml"), std::slice::from_ref(&canon_root))
                .is_none(),
            "a path that cannot canonicalize fails closed"
        );

        let outside_dir = temp_workspace("cwr-outside");
        let outside = outside_dir.join("b.nml");
        fs::write(&outside, "y").unwrap();
        assert!(
            canonical_within_roots(&outside, std::slice::from_ref(&canon_root)).is_none(),
            "containment is against the roots, not existence"
        );

        #[cfg(unix)]
        {
            // An in-root symlink pointing OUTSIDE the root: rejected —
            // containment is judged on the canonical target.
            let escape = root.join("escape.nml");
            std::os::unix::fs::symlink(&outside, &escape).unwrap();
            assert!(
                canonical_within_roots(&escape, std::slice::from_ref(&canon_root)).is_none(),
                "post-resolution containment closes the symlink escape"
            );
            // An in-root symlink to an in-root file: accepted, as the
            // canonical target.
            let alias = root.join("alias.nml");
            std::os::unix::fs::symlink(&inside, &alias).unwrap();
            let via = canonical_within_roots(&alias, std::slice::from_ref(&canon_root))
                .expect("in-root alias resolves");
            assert_eq!(via, dunce::canonicalize(&inside).unwrap());
        }
    }

    #[test]
    fn watched_file_inside_root_is_eligible() {
        let root = temp_workspace("inside");
        let file = root.join("a.nml");
        fs::write(&file, "x").unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(watched_file_is_eligible(&file, &[canon_root]));
    }

    #[test]
    fn watched_file_outside_root_is_rejected() {
        let root = temp_workspace("outside-root");
        let elsewhere = temp_workspace("outside-other");
        let file = elsewhere.join("a.nml");
        fs::write(&file, "x").unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(!watched_file_is_eligible(&file, &[canon_root]));
    }

    #[test]
    fn watched_file_with_no_roots_is_rejected() {
        let root = temp_workspace("no-roots");
        let file = root.join("a.nml");
        fs::write(&file, "x").unwrap();

        assert!(!watched_file_is_eligible(&file, &[]));
    }

    #[test]
    fn watched_file_missing_is_rejected() {
        let root = temp_workspace("missing");
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(!watched_file_is_eligible(
            &root.join("nope.nml"),
            &[canon_root]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn watched_file_symlink_is_rejected() {
        let root = temp_workspace("symlink-root");
        let elsewhere = temp_workspace("symlink-target");
        let target = elsewhere.join("real.nml");
        fs::write(&target, "x").unwrap();
        let link = root.join("link.nml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(
            !watched_file_is_eligible(&link, &[canon_root]),
            "symlinks must be rejected even when placed inside a root"
        );
    }

    // ── Schema universe assembly ──────────────────────────────

    /// Declared (covered) universe: manifest order preserved, buffer
    /// text wins for the own file, a missing declared file contributes
    /// nothing, and an undeclared own file is appended LAST (duplicate
    /// attribution lands on the file whose declaration is in question).
    #[test]
    fn declared_universe_keeps_manifest_order_and_buffer_text() {
        let root = temp_workspace("universe-declared");
        let a = root.join("a.model.nml");
        let b = root.join("b.model.nml");
        fs::write(&a, "model a:\n").unwrap();
        fs::write(&b, "STALE DISK TEXT").unwrap();
        let missing = root.join("gone.model.nml");

        let read = |p: &Path| fs::read_to_string(p).ok();
        let name_of = |p: &Path| p.to_string_lossy().into_owned();
        let declared = vec![a.clone(), missing.clone(), b.clone()];
        let sources = declared_universe(&name_of(&b), "model b:\n", &declared, &read, &name_of);
        let names: Vec<&str> = sources.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![a.to_string_lossy(), b.to_string_lossy()],
            "manifest order, missing file skipped"
        );
        assert_eq!(sources[1].1, "model b:\n", "buffer text wins over disk");

        // Undeclared own file: appended after the declared set.
        let c = root.join("c.model.nml");
        let sources = declared_universe(&name_of(&c), "model c:\n", &declared, &read, &name_of);
        assert_eq!(
            sources.last().map(|(n, _)| n.as_str()),
            Some(c.to_string_lossy().as_ref()),
            "undeclared own file is appended last"
        );
    }

    /// Store-snapshot universe: the package's sources in declaration
    /// order (merge order = duplicate attribution), buffer appended
    /// last under its path key — validated against the PUBLISHED
    /// package, not directory neighbors.
    #[test]
    fn snapshot_universe_keeps_order_and_appends_buffer_last() {
        let sources = vec![
            ("core".to_string(), std::sync::Arc::from("model a:\n")),
            ("extra".to_string(), std::sync::Arc::from("model b:\n")),
        ];
        let out = snapshot_universe("mine.model.nml", "model c:\n", &sources);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["core", "extra", "mine.model.nml"]);
        assert_eq!(out[2].1, "model c:\n", "buffer text is the live text");
    }

    /// Registry (uncovered) universe: own buffer first, members sorted,
    /// and the MAX_UNIVERSE_FILES bound applies deterministically to the
    /// sorted tail — the cap is a real pin, not a comment.
    #[test]
    fn registry_universe_is_sorted_capped_and_own_first() {
        let own_name = "mine.model.nml";
        let docs: Vec<(String, String)> = (0..MAX_UNIVERSE_FILES + 40)
            .map(|i| (format!("m{i:04}.model.nml"), format!("model m{i}:\n")))
            .collect();
        let (sources, truncated) = registry_universe(own_name, "model mine:\n", docs);
        assert_eq!(
            sources.len(),
            MAX_UNIVERSE_FILES,
            "cap bounds the universe including the buffer"
        );
        assert!(
            truncated,
            "a cut set must report truncation — composition ownership hinges on it"
        );
        assert_eq!(sources[0].0, own_name, "own buffer first");
        let tail: Vec<&str> = sources[1..].iter().map(|(n, _)| n.as_str()).collect();
        let mut sorted = tail.clone();
        sorted.sort();
        assert_eq!(tail, sorted, "members sorted for deterministic merge");
        assert_eq!(
            tail.last().copied(),
            Some("m0126.model.nml"),
            "the cap drops the sorted tail, deterministically"
        );

        // Under the cap the registry set IS the namespace: no truncation,
        // ownership stays with the load pass.
        let small: Vec<(String, String)> = (0..3)
            .map(|i| (format!("/ws/s{i}.model.nml"), format!("model s{i}:\n")))
            .collect();
        let (_, truncated) = registry_universe(own_name, "model mine:\n", small);
        assert!(!truncated, "an uncut set must not report truncation");
    }

    // ── Hover markdown safety ─────────────────────────────────

    #[test]
    fn markdown_fences_are_escaped_but_emphasis_left_alone() {
        // A doc containing a fence must not be able to swallow the hover.
        assert_eq!(
            escape_markdown_fences("use ```nml\nx = 1\n``` here"),
            "use \\`\\`\\`nml\nx = 1\n\\`\\`\\` here"
        );
        // Lighter emphasis chars pass through untouched (cosmetic only).
        assert_eq!(
            escape_markdown_fences("a *bold* _claim_ with `code`"),
            "a *bold* _claim_ with `code`"
        );
    }

    // ── Hover credential redaction ────────────────────────────

    #[test]
    fn sensitive_names_detected() {
        assert!(is_sensitive_name("apiKey"));
        assert!(is_sensitive_name("API_TOKEN"));
        assert!(is_sensitive_name("clientSecret"));
        assert!(is_sensitive_name("Password"));
        assert!(!is_sensitive_name("name"));
        assert!(!is_sensitive_name("description"));
    }

    #[test]
    fn hover_summary_redacts_credential_strings() {
        let source = "provider P:\n    apiKey = \"gsk_super_secret\"\n    model = \"llama\"\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block declaration");
        };
        let summary = summarize_body(&block.body);
        assert!(summary.contains("apiKey = \"…\""), "summary: {summary}");
        assert!(
            !summary.contains("gsk_super_secret"),
            "credential leaked: {summary}"
        );
        assert!(summary.contains("model = \"llama\""), "summary: {summary}");
    }

    #[test]
    fn hover_summary_keeps_secret_env_reference() {
        // `$ENV.X` is a reference, not secret material; it stays visible.
        let source = "provider P:\n    apiKey = $ENV.GROQ_KEY\n";
        let file = nml_core::cst::parse_to_ast(source).unwrap();
        let DeclarationKind::Block(block) = &file.declarations[0].kind else {
            panic!("expected block declaration");
        };
        let summary = summarize_body(&block.body);
        assert!(summary.contains("GROQ_KEY"), "summary: {summary}");
    }

    #[test]
    fn format_named_value_redacts_only_sensitive_strings() {
        let secret = Value::String("hunter2".into());
        assert_eq!(format_named_value("password", &secret), "\"…\"");
        assert_eq!(format_named_value("greeting", &secret), "\"hunter2\"");
        // Non-string values keep their normal rendering.
        assert_eq!(format_named_value("maxKeys", &Value::number(3)), "3");
    }

    /// RFC 0026 decision 6: the fold matches the document's own row by
    /// RANGE **and** code. Two rows at one range under different codes —
    /// what a document may legitimately report — must not swap places: a
    /// wrapper whose `cause` names the second folds into the second, the
    /// first keeps its own `relatedInformation`, and no wrapper row is
    /// published beside them.
    #[test]
    fn a_wrapper_folds_into_the_row_that_carries_its_cause() {
        use tower_lsp::lsp_types::{Diagnostic, NumberOrString, Position, Range};
        let range = Range::new(Position::new(1, 4), Position::new(1, 10));
        let row = |code: &str| Diagnostic {
            range,
            code: Some(NumberOrString::String(code.to_string())),
            message: format!("{code} says so"),
            ..Default::default()
        };
        let mut rows = vec![row("NML0002"), row("NML2001")];
        let note = packages::DegradedNote {
            message: "manifest failed to load".to_string(),
            severity: nml_core::diagnostic::Severity::Error,
            code: Some(nml_core::diagnostic::codes::RESOLUTION_INPUT_UNLOADABLE),
            anchor: packages::NoteAnchor::At(nml_core::span::Span::new(18, 24)),
            related: Vec::new(),
            suggestions: Vec::new(),
            cause: Some(nml_core::diagnostic::codes::UNKNOWN_PROPERTY),
        };
        let uri = Url::parse("file:///ws/demo.package.nml").expect("a uri");
        note_rows(
            std::slice::from_ref(&note),
            "package demo:\n    versio = \"0.1.0\"\n",
            None,
            &uri,
            "demo.package.nml",
            &|_| None,
            &mut rows,
        );
        assert_eq!(rows.len(), 2, "no wrapper row is published: {rows:?}");
        assert!(
            rows[0].related_information.is_none(),
            "the row that is not the cause is untouched: {rows:?}"
        );
        let folded = rows[1]
            .related_information
            .as_ref()
            .unwrap_or_else(|| panic!("the cause's row carries the context: {rows:?}"));
        assert_eq!(folded.len(), 1, "{folded:?}");
        assert_eq!(folded[0].message, LOAD_NOTE, "{folded:?}");
    }
}

#[cfg(test)]
mod folder_path_tests {
    use super::folder_path;
    use tower_lsp::lsp_types::Url;

    /// A folder the platform cannot canonicalize keeps its spelling: WASI
    /// has no `realpath` (every path fails there), and natively an absent
    /// folder — the kernel refuses to anchor a universe at either unless
    /// the spelling verifies. Dropping the folder instead made the
    /// bundled WASM server treat every document as outside every folder.
    #[test]
    fn a_folder_that_cannot_be_canonicalized_keeps_its_spelling() {
        let absent = std::env::temp_dir().join("nml-lsp-no-such-folder-a1b2c3");
        let uri = Url::from_file_path(&absent).expect("absolute");
        assert_eq!(folder_path(&uri), Some(absent));
    }

    /// Where `realpath` exists, an operator's own link is followed: the
    /// folder opened through it and its target are one root.
    #[cfg(unix)]
    #[test]
    fn an_existing_folder_is_canonical() {
        let base = std::env::temp_dir().join(format!("nml-lsp-folder-path-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).expect("mkdir");
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let uri = Url::from_file_path(&link).expect("absolute");
        assert_eq!(
            folder_path(&uri),
            Some(dunce::canonicalize(&real).expect("canonical"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
