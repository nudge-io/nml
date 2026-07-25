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
use nml_core::types::Value;
use nml_core::{FieldTarget, SchemaIndex};
use nml_validate::schema::MembershipSemantics;

use crate::diagnostics::{self, SchemaMode};
use crate::packages::{self, Resolution, WorkspaceView};
use crate::position::{self, LineIndex};

const MAX_DIR_DEPTH: usize = 20;
const MAX_FILE_COUNT: usize = 10_000;

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

pub struct Inner {
    documents: Mutex<HashMap<Url, String>>,
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
    scoped_models: Mutex<HashMap<String, Vec<ModelDef>>>,
    scoped_enums: Mutex<HashMap<String, Vec<EnumDef>>>,
    scoped_oneofs: Mutex<HashMap<String, Vec<OneOfDef>>>,
    project_config: Mutex<nml_core::ProjectConfig>,
    /// Canonicalized workspace roots captured at initialize; watched-file
    /// events outside these roots are ignored.
    workspace_roots: Mutex<Vec<PathBuf>>,
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
}

pub struct NmlLanguageServer {
    client: Client,
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
            client,
            inner: Arc::new(Inner {
                documents: Mutex::new(HashMap::new()),
                diags_cache: Mutex::new(HashMap::new()),
                indexed_uris: Mutex::new(HashSet::new()),
                open_docs: Mutex::new(HashSet::new()),
                scoped_models: Mutex::new(HashMap::new()),
                scoped_enums: Mutex::new(HashMap::new()),
                scoped_oneofs: Mutex::new(HashMap::new()),
                project_config: Mutex::new(nml_core::ProjectConfig::default()),
                workspace_roots: Mutex::new(Vec::new()),
                membership,
                resolver: packages::PackageResolver::with_injected(
                    store,
                    store_events_tx,
                    injected,
                ),
                insert_replace_support: std::sync::atomic::AtomicBool::new(false),
                label_details_support: std::sync::atomic::AtomicBool::new(false),
                explain_command: Mutex::new(None),
            }),
            store_events: Mutex::new(store_events_rx),
        }
    }
}

impl Inner {
    fn find_nml_files(dir: &Path, files: &mut Vec<std::path::PathBuf>, depth: usize) {
        if depth > MAX_DIR_DEPTH || files.len() >= MAX_FILE_COUNT {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let is_symlink = entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false);
            if is_symlink {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name != "node_modules" && name != ".git" && !name.starts_with('.') {
                    Self::find_nml_files(&path, files, depth + 1);
                }
            } else if path.extension().is_some_and(|e| e == "nml") {
                files.push(path);
                if files.len() >= MAX_FILE_COUNT {
                    return;
                }
            }
        }
    }

    fn index_workspace(&self, roots: &[Url]) {
        let mut docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
        let mut indexed = self.indexed_uris.lock().unwrap_or_else(|e| e.into_inner());
        for root in roots {
            let path = match root.to_file_path() {
                Ok(p) => p,
                Err(_) => continue,
            };

            let project_file = path.join("nml-project.nml");
            if project_file.exists() {
                if let Ok(content) = fs::read_to_string(&project_file) {
                    let file = nml_core::cst::parse_best_effort(&content);
                    let config = nml_core::ProjectConfig::from_file(&file);
                    *self
                        .project_config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = config;
                    if let Ok(uri) = Url::from_file_path(&project_file) {
                        docs.insert(uri.clone(), content);
                        indexed.insert(uri);
                    }
                }
            }

            let mut files = Vec::new();
            Self::find_nml_files(&path, &mut files, 0);
            for path in files {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(uri) = Url::from_file_path(&path) {
                        docs.insert(uri.clone(), content);
                        indexed.insert(uri);
                    }
                }
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
            if !uri.as_str().ends_with(".model.nml") {
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
        let nearest = uri.to_file_path().ok().and_then(|p| {
            let p = dunce::canonicalize(&p).unwrap_or(p);
            let roots = self
                .workspace_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let doc_text = |path: &Path| -> Option<String> {
                let uri = Url::from_file_path(path).ok()?;
                self.documents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&uri)
                    .cloned()
            };
            let view = WorkspaceView {
                roots: &roots,
                manifests: &[],
                doc_text: &doc_text,
            };
            packages::nearest_project_config(&p, &view).map(|(_, config)| config)
        });
        let mut config = match nearest {
            Some(pc) => self.config_from_project(&pc),
            None => self.diagnostic_config(),
        };
        config.uri_is_registry_source = uri.as_str().ends_with(".model.nml");
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
        }
    }

    fn diagnostic_config(&self) -> diagnostics::DiagnosticConfig {
        let pc = self
            .project_config
            .lock()
            .unwrap_or_else(|e| e.into_inner());

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
        }
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
        // Roots are canonicalized at initialize; an un-canonicalized document
        // path (macOS /tmp → /private/tmp, symlinked checkouts) would fail
        // every starts_with, silently unrooting resolution — and letting the
        // ancestor walk escape the workspace.
        let path = dunce::canonicalize(&path).unwrap_or(path);
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let manifests: Vec<(PathBuf, String)> = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            docs.iter()
                .filter(|(u, _)| u.as_str().ends_with(".package.nml"))
                .filter_map(|(u, text)| {
                    let p = u.to_file_path().ok()?;
                    // Canonicalized like the resolved document path — a
                    // symlinked workspace must not break is_self/root checks.
                    Some((dunce::canonicalize(&p).unwrap_or(p), text.clone()))
                })
                .collect()
        };
        let doc_text = |p: &Path| -> Option<String> {
            let uri = Url::from_file_path(p).ok()?;
            self.documents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&uri)
                .cloned()
        };
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &doc_text,
        };
        Some(f(&path, &view))
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

    /// Full validation of one document: package-bound (exclusive validator +
    /// binding identity) when a package claims it, the scope-registry path
    /// otherwise, plus any degraded-state notes pinned to the top of file.
    fn validate_document(&self, uri: &Url, text: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let dc = self.diagnostic_config_for(uri);
        let resolved = self.resolve_document(uri);
        let mut diags = match resolved.as_ref().map(|r| &r.resolution) {
            Some(Resolution::Bound(b)) => {
                let identity = b.identity();
                diagnostics::compute(
                    text,
                    &SchemaMode::Package {
                        validator: &b.validator,
                        identity,
                    },
                    &dc,
                    Some(uri),
                )
            }
            _ => {
                let (models, enums, oneofs) = self.models_for_file(uri);
                diagnostics::compute(
                    text,
                    &SchemaMode::Registry {
                        models: &models,
                        enums: &enums,
                        oneofs: &oneofs,
                    },
                    &dc,
                    Some(uri),
                )
            }
        };
        if let Some(resolved) = &resolved {
            let top = tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(0, 0),
                tower_lsp::lsp_types::Position::new(0, 0),
            );
            for note in &resolved.notes {
                let line_index = LineIndex::new(text);
                let range = note.span.map(|sp| line_index.range(sp)).unwrap_or(top);
                diags.push(tower_lsp::lsp_types::Diagnostic {
                    range,
                    severity: Some(if note.warning {
                        tower_lsp::lsp_types::DiagnosticSeverity::WARNING
                    } else {
                        tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION
                    }),
                    message: note.message.clone(),
                    source: Some("nml".to_string()),
                    ..Default::default()
                });
            }
        }
        // Schema-source pass (RFC 0030): a covered `.model.nml` is validated
        // *as a schema* — extraction errors, directive vocabulary, sibling
        // info. Uncovered model files stay opaque (no vocabulary_for match ⇒
        // nothing appended). The pass re-derives extraction errors that
        // `compute` already emitted as parse errors, so exact duplicates
        // (same range, message, severity) are suppressed rather than
        // double-squiggled.
        if uri.as_str().ends_with(".model.nml") {
            match self.vocabulary_for_document(uri) {
                packages::VocabularyOutcome::Covered(vocab) => {
                    for diag in diagnostics::schema_source_pass(text, &vocab, Some(uri)) {
                        let duplicate = diags.iter().any(|d| {
                            d.range == diag.range
                                && d.message == diag.message
                                && d.severity == diag.severity
                        });
                        if !duplicate {
                            diags.push(diag);
                        }
                    }
                }
                // The bounded claims walk hit its cap: coverage is honestly
                // unknown, so say so ONCE (info, top of file) and name the
                // remedy — declaring the file makes coverage walk-free.
                // Multiple candidates ⇒ no name (guessing one would mislead).
                packages::VocabularyOutcome::Undetermined { candidates } => {
                    let name = match candidates.as_slice() {
                        [single] => format!("'{single}'? "),
                        _ => String::new(),
                    };
                    diags.push(tower_lsp::lsp_types::Diagnostic {
                        range: tower_lsp::lsp_types::Range::new(
                            tower_lsp::lsp_types::Position::new(0, 0),
                            tower_lsp::lsp_types::Position::new(0, 0),
                        ),
                        severity: Some(tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION),
                        message: format!(
                            "package coverage undetermined ({name}root exceeds the scan bound); \
                             declare this file in the package's []schema to get directive vocabulary"
                        ),
                        source: Some("nml".to_string()),
                        ..Default::default()
                    });
                }
                // Definitively uncovered files stay silent — plain-nml
                // schema authors are never punished for the mechanism.
                packages::VocabularyOutcome::Opaque => {}
            }
        }
        diags
    }
}

impl NmlLanguageServer {
    /// Surface store-health transitions (Ready↔Failed, shadow warnings) the
    /// resolver queued during resolution, as `window/logMessage`. Called from
    /// the document-pull handler — the frequent path that holds the `Client` —
    /// so it replaces the deleted background notifier. Drain fully under the
    /// lock into a `Vec`, then log outside it (never hold a lock across await).
    /// This document's diagnostics via the RFC 0010 tier-1 cache — computed
    /// at most once per text state, by whichever consumer asks first (the
    /// document pull or hover's explanation lookup). The entry is validated
    /// against the CURRENT buffer text, so an insert from a compute that
    /// raced an edit reads as a miss — never served as stale ranges. `None`
    /// for an unknown document.
    async fn cached_diagnostics(
        &self,
        uri: &Url,
    ) -> Option<Arc<Vec<tower_lsp::lsp_types::Diagnostic>>> {
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
            if entry.text == text && entry.generation == generation {
                return Some(Arc::clone(&entry.items));
            }
        }
        let items = Arc::new(self.validate_document(uri, &text));
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
        Some(items)
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
        let items = self.cached_diagnostics(uri).await?;
        explanations_at_position(&items, pos)
    }

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
    fn on_change(&self, uri: Url, text: String) {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.clone(), text.clone());
        // This document's cached diagnostics are stale (text changed). The
        // project-config and registry branches below clear wholesale — those
        // changes affect every document.
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);

        if uri.as_str().ends_with("nml-project.nml") {
            let file = nml_core::cst::parse_best_effort(&text);
            let config = nml_core::ProjectConfig::from_file(&file);
            *self
                .project_config
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = config;
            // Project config (modifiers, template namespaces) shapes every
            // document's diagnostics — wholesale invalidation.
            self.diags_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            return;
        }
        if uri.as_str().ends_with(".model.nml") {
            self.rebuild_schema_registry();
        }
    }
}

impl Inner {
    fn find_definition(
        &self,
        name: &str,
        current_uri: &Url,
        enclosing_keyword: Option<&str>,
    ) -> Option<(Url, Range)> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        find_definition_in_docs(&docs, name, current_uri, enclosing_keyword)
    }

    fn find_schema_definition(&self, name: &str, current_uri: &Url) -> Option<(Url, Range)> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let file_scope = extract_file_scope(current_uri.as_str());

        let mut model_uris: Vec<&Url> = docs
            .keys()
            .filter(|u| u.as_str().ends_with(".model.nml"))
            .collect();

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
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        find_tagged_ref_definition_in_docs(&docs, role_ref)
    }

    fn find_tagged_ref_hover(&self, keyword: &str, name: &str) -> Option<String> {
        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
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

/// Whether a watched-file event should be honored.
///
/// Mirrors the safety rules of `index_workspace`/`find_nml_files`: the path
/// must not be a symlink, and it must canonicalize to a location inside one
/// of the (canonicalized) workspace roots. Clients can send arbitrary
/// `file://` URIs in watched-file notifications, so this is the boundary
/// check that keeps the server from reading files outside the workspace.
fn watched_file_is_eligible(path: &Path, roots: &[PathBuf]) -> bool {
    let is_symlink = fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true);
    if is_symlink {
        return false;
    }
    match dunce::canonicalize(path) {
        Ok(canonical) => roots.iter().any(|root| canonical.starts_with(root)),
        Err(_) => false,
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

fn extract_schema_scope(uri_str: &str) -> String {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    filename
        .strip_suffix(".model.nml")
        .unwrap_or("")
        .to_string()
}

fn extract_file_scope(uri_str: &str) -> Option<String> {
    let filename = uri_str.rsplit('/').next().unwrap_or(uri_str);
    if filename.ends_with(".model.nml") {
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
            let mut model_uris: Vec<&Url> = docs
                .keys()
                .filter(|u| u.as_str().ends_with(".model.nml"))
                .collect();

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
            if !uri.as_str().ends_with(".model.nml") {
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
                            span_to_range(arm.model.span, line_index),
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
            _ => {}
        }
    }
    symbols
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
fn value_position_prop_name(source: &str, pos: Position) -> Option<&str> {
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
struct ValueGovernors<'i> {
    fields: Vec<&'i FieldDef>,
    discriminator_arms: Vec<String>,
}

fn value_governors_at<'i>(
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
    let FieldTarget::Model(model) = index.resolve_ref(&block.keyword.name) else {
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
    let block = file.declarations.iter().find_map(|decl| {
        let range = span_to_range(decl.span, line_index);
        if pos.line > range.start.line && pos.line <= range.end.line {
            if let DeclarationKind::Block(b) = &decl.kind {
                return Some(b);
            }
        }
        None
    })?;
    let FieldTarget::Model(model) = index.resolve_ref(&block.keyword.name) else {
        return None;
    };
    arm_target_descend(model, &block.body, pos, index, line_index)
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
    for entry in &body.entries {
        let BodyEntryKind::NestedBlock(nested) = &entry.kind else {
            continue;
        };
        let range = span_to_range(entry.span, line_index);
        if pos.line <= range.start.line || pos.line > range.end.line {
            continue;
        }
        let field = model.fields.iter().find(|f| f.name == nested.name.name)?;
        return match index.resolve_type_in_body(&field.field_type, &nested.body) {
            FieldTarget::Arms { target, .. } => Some(named_type_names(target)),
            FieldTarget::Model(child) => {
                arm_target_descend(child, &nested.body, pos, index, line_index)
            }
            _ => None,
        };
    }
    None
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
                        FieldTarget::Model(m) => Some(NameableVariant::Model(m)),
                        FieldTarget::OneOf(o) => Some(NameableVariant::OneOf(o)),
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
    let mut owned: Option<&nml_core::ast::NestedBlock> = None;
    for (i, entry) in body.entries.iter().enumerate() {
        let BodyEntryKind::NestedBlock(nested) = &entry.kind else {
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
            owned = Some(nested);
        }
    }
    if let Some(nested) = owned {
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
            FieldTarget::ListOf(_) | FieldTarget::SetOf(_) => {
                let base = match &field.field_type {
                    FieldType::Modifier(inner) => inner.as_ref(),
                    t => t,
                };
                let (FieldType::List(elem_ty) | FieldType::Set(elem_ty)) = base else {
                    return None;
                };
                let mut item_owner: Option<&ListItem> = None;
                for (i, e) in nested.body.entries.iter().enumerate() {
                    let BodyEntryKind::ListItem(item) = &e.kind else {
                        continue;
                    };
                    let r = span_to_range(e.span, line_index);
                    if pos.line <= r.start.line {
                        continue;
                    }
                    let next_start = nested
                        .body
                        .entries
                        .get(i + 1)
                        .map(|e| span_to_range(e.span, line_index).start.line);
                    let bounded = match next_start {
                        Some(next) => pos.line < next,
                        None => true,
                    };
                    if bounded {
                        item_owner = Some(item);
                    }
                }
                let item = item_owner?;
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
            FieldTarget::Union => {
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
        FieldTarget::Model(m) => Some(m),
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
        Value::String(s) | Value::Duration(s) | Value::Path(s) => format!("{s:?}"),
        Value::Number(n) => n.to_string(),
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
/// insert-text sigils — the round-20 layer's central concept, named once.
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
        FieldTarget::Leaf => format!("{sigil}{} = ", field.name),
        _ => format!("{sigil}{}:", field.name),
    }
}

/// Parse the wire `suggestions` payload of a diagnostic's `data` (see the
/// diagnostics.rs producer): every valid `{replacement, start, end, kind}`
/// entry, capped at the producer's own alternative bound
/// (`MAX_FIX_ALTERNATIVES` in nml-validate). Parse-then-cap, in that order:
/// the cap counts VALID entries — so a hostile/buggy client can neither mint
/// unbounded actions from one diagnostic nor bury a legitimate entry behind
/// malformed padding — and the singleton-preferred gate downstream counts
/// only real suggestions.
fn parse_suggestion_entries(
    suggestions: &[serde_json::Value],
) -> Vec<(String, usize, usize, String)> {
    const MAX_SUGGESTION_ACTIONS: usize = 8;
    suggestions
        .iter()
        .filter_map(|s| {
            let start = s.get("start")?.as_u64()? as usize;
            let end = s.get("end")?.as_u64()? as usize;
            // An inverted span would round-trip as a spec-invalid LSP Range;
            // fail closed like every other malformed entry.
            (start <= end).then_some(())?;
            Some((
                s.get("replacement")?.as_str()?.to_string(),
                start,
                end,
                s.get("kind")?.as_str()?.to_string(),
            ))
        })
        .take(MAX_SUGGESTION_ACTIONS)
        .collect()
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
        FieldTarget::OneOf(oneof) if oneof.discriminator == prop_name => Some(oneof),
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

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Reference(r) => r.clone(),
        Value::Secret(s) => s.clone(),
        Value::Role(r) => r.clone(),
        Value::Duration(d) => format!("\"{}\"", d),
        Value::Path(p) => format!("\"{}\"", p),
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
        let count = line.matches("\"\"\"").count();
        for _ in 0..count {
            open = !open;
        }
    }
    open
}

/// Compute the desired indentation (in spaces) for a new line inserted after
/// `line_idx` in the given source lines.  This drives `onTypeFormatting` for
/// the `\n` trigger so the cursor lands at the right column.
fn compute_indent_after_line(lines: &[&str], line_idx: usize) -> usize {
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
        return prev_indent + 4;
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
    let quoted = format!("\"{variant}\"");
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

impl NmlLanguageServer {
    /// Build the workspace edit for a pin/opt-out action targeting the
    /// nearest `nml-project.nml` (structural CST insert into the existing
    /// `project` block — RFC 0030 P2) or creating one at the binding's root.
    /// Injection-safe twice over: package names are charset-constrained at
    /// package load, and the CST splice refuses any snippet that does not
    /// parse as plain body entries.
    fn project_edit_action(
        &self,
        file_path: &Path,
        root: &Path,
        title: String,
        edit: ProjectEdit,
    ) -> Option<CodeAction> {
        // The action writes files: it must never target a path outside the
        // workspace (a root marker in `$HOME` must not make the editor
        // create `~/nml-project.nml`).
        {
            let roots = self
                .workspace_roots
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !roots
                .iter()
                .any(|r| root.starts_with(r) || r.starts_with(root))
            {
                return None;
            }
        }

        // Nearest existing nml-project.nml between the file and its root.
        let mut existing: Option<PathBuf> = None;
        let mut dir = file_path.parent();
        while let Some(d) = dir {
            let candidate = d.join("nml-project.nml");
            if candidate.is_file() {
                existing = Some(candidate);
                break;
            }
            if d == root {
                break;
            }
            dir = d.parent();
        }

        let workspace_edit = match existing {
            Some(project_path) => {
                // Open-document text wins over disk — an unsaved buffer is
                // the text the computed offsets must be valid against.
                let text = Url::from_file_path(&project_path)
                    .ok()
                    .and_then(|u| {
                        self.documents
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&u)
                            .cloned()
                    })
                    .or_else(|| std::fs::read_to_string(&project_path).ok())?;
                let new_text = project_file_insertion(&text, &edit)?;
                // Whole-document replacement: the CST splice returns the
                // complete new text, and a single full-range TextEdit is the
                // simplest LSP shape that is guaranteed byte-exact — no
                // offset→Position math for a structural edit to get subtly
                // wrong, and the file is a small config so the payload cost
                // is irrelevant.
                let line_index = LineIndex::new(&text);
                let uri = Url::from_file_path(&project_path).ok()?;
                let mut changes = std::collections::HashMap::new();
                changes.insert(
                    uri,
                    vec![TextEdit {
                        range: line_index.range(nml_core::span::Span::new(0, text.len())),
                        new_text,
                    }],
                );
                WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }
            }
            None => {
                let project_path = root.join("nml-project.nml");
                let uri = Url::from_file_path(&project_path).ok()?;
                let content = match &edit {
                    ProjectEdit::Pin(name) => {
                        format!("project Project:\n    schemaPackages:\n        - {name}\n")
                    }
                    ProjectEdit::OptOut => {
                        "project Project:\n    autoAssociate = false\n".to_string()
                    }
                };
                WorkspaceEdit {
                    document_changes: Some(DocumentChanges::Operations(vec![
                        DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                            uri: uri.clone(),
                            options: None,
                            annotation_id: None,
                        })),
                        DocumentChangeOperation::Edit(TextDocumentEdit {
                            text_document: OptionalVersionedTextDocumentIdentifier {
                                uri,
                                version: None,
                            },
                            edits: vec![OneOf::Left(TextEdit {
                                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                                new_text: content,
                            })],
                        }),
                    ])),
                    ..Default::default()
                }
            }
        };

        Some(CodeAction {
            title,
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(workspace_edit),
            ..Default::default()
        })
    }

    /// `nml/schemaInfo` (RFC 0030 introspection): which package validates a
    /// file, from where, at which hash, bound how — plus every degraded-state
    /// note. Registered as a custom JSON-RPC method; any LSP client can call
    /// it, the VS Code extension renders it.
    pub async fn schema_info(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let uri = params
            .get("uri")
            .and_then(|u| u.as_str())
            .and_then(|u| Url::parse(u).ok());
        let Some(uri) = uri else {
            return Ok(serde_json::json!({ "error": "missing or invalid 'uri'" }));
        };
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
        let notes: Vec<serde_json::Value> = resolved
            .notes
            .iter()
            .map(|n| {
                serde_json::json!({
                    "message": n.message,
                    "severity": if n.warning { "warning" } else { "info" },
                    "range": n
                        .span
                        .zip(line_index.as_ref())
                        .map(|(sp, li)| serde_json::to_value(li.range(sp)).ok()),
                })
            })
            .collect();
        let roots = self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(match &resolved.resolution {
            Resolution::Bound(b) => serde_json::json!({
                "bound": true,
                "package": b.package_name,
                "version": b.package_version,
                "contentHash": b.content_hash,
                "binding": b.binding_name,
                "source": b.source.label(),
                "step": match b.step {
                    packages::BindingStep::Pinned => "pinned",
                    packages::BindingStep::AutoAssociated => "auto-associated",
                },
                // Workspace-relative — never an absolute host path, and never the
                // `/workspace` WASI mount prefix on the wasm neutral server.
                "root": packages::display_path(&b.root, &roots),
                "shadowsStore": b.shadows_store,
                "actions": if b.step == packages::BindingStep::AutoAssociated
                    && b.source != packages::DefinitionSource::Builtin
                {
                    serde_json::json!(["pin", "disableAutoAssociation"])
                } else {
                    serde_json::json!([])
                },
                "notes": notes,
            }),
            Resolution::Unbound => serde_json::json!({ "bound": false, "notes": notes }),
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

    /// `nml/explainIndex {} → [{ code, summary }]` (RFC 0010 tier 2): every
    /// diagnostic code with its one-line summary, in index order — the
    /// discoverability surface behind the editor's explain-a-code palette.
    /// Deliberately flat: band grouping would promote the allocation bands
    /// into wire API, which they are documented not to be. Params are
    /// accepted and ignored (tolerant of `{}`, `null`, or absent).
    pub async fn explain_index(&self, _params: serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Array(
            nml_core::diagnostic::explain_index()
                .into_iter()
                .map(|(code, summary)| serde_json::json!({ "code": code, "summary": summary }))
                .collect(),
        ))
    }
}

/// Compute the full new text of an **existing** `nml-project.nml` for a
/// pin/opt-out edit via the CST splice API (`nml_core::cst::edit`, RFC 0030
/// P2) — a comment-preserving structural insert, not a line-offset text
/// patch. `None` when the edit is redundant (already pinned / already opted
/// out) or the file has no `project` block to target.
fn project_file_insertion(text: &str, edit: &ProjectEdit) -> Option<String> {
    use nml_core::cst::edit::{insert_entry_at_path, EntryPosition};
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
                    &format!("schemaPackages:\n    - {name}"),
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
        *self
            .workspace_roots
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = roots
            .iter()
            .filter_map(|r| r.to_file_path().ok())
            .filter_map(|p| dunce::canonicalize(&p).ok())
            .collect();
        if !roots.is_empty() {
            self.index_workspace(&roots);
            self.rebuild_schema_registry();
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("NML: indexed {} workspace root(s)", roots.len()),
                )
                .await;
        }
        Ok(InitializeResult {
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
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        // Dynamic capability registration is a server→client *request*. The wasm
        // neutral server (RFC 0035) is driven by a synchronous pump that cannot
        // await one, so file-watch registration is native-only; under
        // `wasm-wasi-core` the editor host drives file events without it.
        #[cfg(not(target_arch = "wasm32"))]
        {
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
            let _ = self.client.register_capability(vec![registration]).await;
        }
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
        let items = self.cached_diagnostics(&uri).await.unwrap_or_default();
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.open_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(params.text_document.uri.clone());
        // State only; the client pulls this document's diagnostics (didOpen
        // triggers a pull under the diagnostic-provider capability).
        self.on_change(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.on_change(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.open_docs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        self.diags_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&uri);
        let was_model = uri.as_str().ends_with(".model.nml");
        let indexed = self
            .indexed_uris
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&uri);
        if !indexed {
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
        // about existing files, so any watched change invalidates them
        // (cheap to recompute; wrong-until-restart is not acceptable).
        if !params.changes.is_empty() {
            self.resolver.invalidate_claims();
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
                    if let Ok(content) = fs::read_to_string(&path) {
                        self.on_change(change.uri, content);
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
                    if change.uri.as_str().ends_with(".model.nml") {
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
        if uri.as_str().ends_with(".model.nml") {
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
                    for d in &vocab.directives {
                        items.push(CompletionItem {
                            label: d.name.clone(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(d.arg.label().to_string()),
                            documentation: Some(Documentation::String(d.doc.clone())),
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
                let namespaces: Vec<String> = {
                    let pc = self
                        .project_config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    pc.template_namespaces.clone()
                };
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
            let (model_ref_types, discriminator_values, value_completions): (
                Vec<String>,
                Option<Vec<String>>,
                Option<ValueCompletions>,
            ) = {
                let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
                match docs
                    .get(&uri)
                    .map(|source| (source, nml_core::cst::parse_best_effort(source)))
                {
                    // Parse once and build one schema index, shared by all detectors.
                    Some((source, file)) => {
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
                        (model_refs, discriminator, values)
                    }
                    None => (Vec::new(), None, None),
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
                // Arm-target position (RFC 0007): after the `->` on an arm
                // line, offer declarations of the arm set's target type `V`
                // instead of field names.
                let after_arrow = position::line_at(source, pos.line)
                    .map(|line| {
                        let end = position::utf16_to_byte(line, pos.character);
                        line[..end].contains("->")
                    })
                    .unwrap_or(false);
                if after_arrow {
                    if let Some(target_keywords) =
                        find_arm_target_types_at(&file, pos, index, &line_index)
                    {
                        for keyword in &target_keywords {
                            // Enum-typed arm target (RFC 0030): offer the
                            // declared variants as values.
                            if let Some(e) = index.enum_def(keyword) {
                                for (i, variant) in e.variants.iter().enumerate() {
                                    items.push(CompletionItem {
                                        label: format!("\"{variant}\""),
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
                                let FieldTarget::Model(model) =
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

            let pc = self
                .project_config
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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
        //    re-derived, never parsed out of message text.
        for diag in &params.context.diagnostics {
            let Some(suggestions) = diag
                .data
                .as_ref()
                .and_then(|d| d.get("suggestions"))
                .and_then(|s| s.as_array())
            else {
                continue;
            };
            let parsed = parse_suggestion_entries(suggestions);
            let total = parsed.len();
            for (replacement, start, end, kind) in parsed {
                let edit = TextEdit {
                    range: line_index.range(nml_core::span::Span::new(start, end)),
                    new_text: replacement.clone(),
                };
                let mut changes = std::collections::HashMap::new();
                changes.insert(uri.clone(), vec![edit]);
                // Titles derive from the suggestion KIND; `is_preferred` only
                // for a SINGLETON did-you-mean — N mutually exclusive fixes
                // must never let the editor auto-apply a guess (the exact
                // ambiguity RFC 0015 D2 exists to forbid).
                let title = if kind == "fix" {
                    format!("Apply fix: `{replacement}`")
                } else {
                    format!("Replace with \"{replacement}\"")
                };
                let preferred = kind == "didYouMean" && total == 1;
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diag.clone()]),
                    edit: Some(WorkspaceEdit {
                        changes: Some(changes),
                        ..Default::default()
                    }),
                    is_preferred: preferred.then_some(true),
                    ..Default::default()
                }));
            }
        }

        // 2. Pin / opt-out on auto-associated documents. Structural CST
        //    inserts (RFC 0030 P2) — injection-safe because package names are
        //    charset-constrained identifiers (enforced at package load) AND
        //    the splice API refuses snippets that don't parse as body entries.
        if let Some(resolved) = self.resolve_document(&uri) {
            if let Resolution::Bound(binding) = &resolved.resolution {
                if binding.step == packages::BindingStep::AutoAssociated
                    && binding.source != packages::DefinitionSource::Builtin
                {
                    if let Ok(path) = uri.to_file_path() {
                        let name = &binding.package_name;
                        if let Some(action) = self.project_edit_action(
                            &path,
                            &binding.root,
                            format!("Pin schema package '{name}'"),
                            ProjectEdit::Pin(name.clone()),
                        ) {
                            actions.push(CodeActionOrCommand::CodeAction(action));
                        }
                        if let Some(action) = self.project_edit_action(
                            &path,
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
        }

        // 3. "Explain NML0000" (RFC 0010 tier 2) — negotiation-gated: emitted
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
                        let summary = format!(
                        "**Schema package:** `{}` {} · `{}` · {} · binding `{}`\n\nroot: `{}`{}",
                        b.package_name,
                        b.package_version,
                        format_args!("blake3:{}", nml_validate::store::hash8(&b.content_hash)),
                        b.source.label(),
                        b.binding_name,
                        packages::display_path(&b.root, &roots),
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

            // Directive hover (RFC 0030/0032): `#name` in a covered model file
            // renders the vocabulary entry. Unknown names get no hover — the
            // vocabulary diagnostic already explains them.
            if uri.as_str().ends_with(".model.nml") {
                if let Some(name) = directive_name_at(line, byte_col) {
                    // Covered files only: without a known vocabulary there is no
                    // entry to render (undetermined coverage already surfaced
                    // through the info diagnostic).
                    if let packages::VocabularyOutcome::Covered(vocab) =
                        self.vocabulary_for_document(&uri)
                    {
                        if let Some(d) = vocab.directives.iter().find(|d| d.name == name) {
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
                                        escape_markdown_fences(&d.doc)
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
                let file = nml_core::cst::parse_best_effort(&source_clone);
                let line_index = LineIndex::new(&source_clone);
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
                    "number" => Some("**number** -- General-purpose numeric (integer or decimal)"),
                    "money" => Some(
                        "**money** -- Exact currency value with ISO 4217 code (e.g., `19.99 USD`)",
                    ),
                    "bool" => Some("**bool** -- Boolean value (`true` or `false`)"),
                    "duration" => {
                        Some("**duration** -- Time duration (e.g., `\"72h\"`, `\"30s\"`)")
                    }
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
                    let file = nml_core::cst::parse_best_effort(&source_clone);
                    let handle = self.schema_index_for(&uri);
                    let line_index = LineIndex::new(&source_clone);
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

        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut locations = Vec::new();

        for (doc_uri, source) in &docs {
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

        let (word, source_clone) = {
            let docs = self.documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(source) = docs.get(&uri) else {
                return Ok(None);
            };
            let Some(line) = position::line_at(source, pos.line) else {
                return Ok(None);
            };
            (
                extract_word_at(line, position::utf16_to_byte(line, pos.character)),
                source.clone(),
            )
        };

        if word.is_empty() {
            return Ok(None);
        }

        let line_index = LineIndex::new(&source_clone);
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
            Err(_) => return Ok(None),
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

        let desired = compute_indent_after_line(&lines, prev_line_idx);
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

        let docs: HashMap<Url, String> = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        for (doc_uri, source) in &docs {
            let line_index = LineIndex::new(source);
            let refs = find_references_in_source(source, &word, &line_index);
            if !refs.is_empty() {
                changes.insert(
                    doc_uri.clone(),
                    refs.into_iter()
                        .map(|range| TextEdit {
                            range,
                            new_text: new_name.clone(),
                        })
                        .collect(),
                );
            }
        }

        if changes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }))
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
        let source =
            "workflow W:\n    entrypoint = \"start\"\n    steps:\n        - s1:\n            provider = GroqFast\n";
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
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
        assert_eq!(compute_indent_after_line(&lines, 1), 8);
    }

    #[test]
    fn indent_after_list_item_colon() {
        let lines = vec!["    steps:", "        - classify:"];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
    }

    #[test]
    fn indent_after_property() {
        let lines = vec!["        - classify:", "            provider = Groq"];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
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
        assert_eq!(compute_indent_after_line(&lines, 4), 20);
    }

    #[test]
    fn indent_after_blank_line_uses_prev_non_empty() {
        let lines = vec!["    steps:", "        - classify:", ""];
        assert_eq!(compute_indent_after_line(&lines, 2), 12);
    }

    #[test]
    fn indent_after_nested_block_colon() {
        let lines = vec!["        - router:", "            routes:"];
        assert_eq!(compute_indent_after_line(&lines, 1), 16);
    }

    #[test]
    fn indent_inside_triple_quote() {
        let lines = vec![
            "            system = \"\"\"",
            "            You are a helpful assistant.",
        ];
        assert_eq!(compute_indent_after_line(&lines, 1), 12);
    }

    #[test]
    fn indent_after_scalar_list_item() {
        let lines = vec!["enum providerType:", "    - \"anthropic\""];
        assert_eq!(compute_indent_after_line(&lines, 1), 4);
    }

    #[test]
    fn indent_after_comment_ending_with_colon() {
        let lines = vec!["    // this is a comment:"];
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
    }

    #[test]
    fn indent_empty_source() {
        let lines: Vec<&str> = vec![];
        assert_eq!(compute_indent_after_line(&lines, 0), 0);
    }

    #[test]
    fn indent_at_top_level() {
        let lines = vec!["workflow RecipeAssistant:"];
        assert_eq!(compute_indent_after_line(&lines, 0), 4);
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
            at("enum modeKind:\n    - fast\n    - slow\nmodel modelA:\n    mode string?\nmodel modelB:\n    mode modeKind?\nmodel host:\n    slot (modelA | modelB)?\n"),
            Some(vec!["fast".into(), "slow".into()]),
            "a string declaration in an earlier variant must not suppress a later variant's enum"
        );
        // Two enum declarations: variants union in candidate order.
        assert_eq!(
            at("enum kindA:\n    - a1\n    - a2\nenum kindB:\n    - b1\n    - b2\nmodel modelA:\n    mode kindA?\nmodel modelB:\n    mode kindB?\nmodel host:\n    slot (modelA | modelB)?\n"),
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
        assert_eq!(parsed[0].0, "real");
        let nine: Vec<serde_json::Value> = (0..9).map(|i| valid(&format!("s{i}"))).collect();
        let capped = parse_suggestion_entries(&nine);
        assert_eq!(capped.len(), 8);
        assert_eq!(capped[0].0, "s0");
        assert_eq!(capped[7].0, "s7");
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
            let FieldTarget::Model(model) = idx.resolve_ref(&block.keyword.name) else {
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

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        // pid + process-wide counter: pid alone collides when a re-used pid
        // (or a same-process re-entry) hits the same tag.
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nml-lsp-{tag}-{}-{nonce}", std::process::id()));
        // Defensive: a prior run's leftovers (same pid recycled after a crash
        // skipped the test's cleanup) must not leak stale files into this run.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn watched_file_inside_root_is_eligible() {
        let root = temp_workspace("inside");
        let file = root.join("a.nml");
        fs::write(&file, "x").unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(watched_file_is_eligible(&file, &[canon_root]));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn watched_file_outside_root_is_rejected() {
        let root = temp_workspace("outside-root");
        let elsewhere = temp_workspace("outside-other");
        let file = elsewhere.join("a.nml");
        fs::write(&file, "x").unwrap();
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(!watched_file_is_eligible(&file, &[canon_root]));

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&elsewhere).ok();
    }

    #[test]
    fn watched_file_with_no_roots_is_rejected() {
        let root = temp_workspace("no-roots");
        let file = root.join("a.nml");
        fs::write(&file, "x").unwrap();

        assert!(!watched_file_is_eligible(&file, &[]));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn watched_file_missing_is_rejected() {
        let root = temp_workspace("missing");
        let canon_root = dunce::canonicalize(&root).unwrap();

        assert!(!watched_file_is_eligible(
            &root.join("nope.nml"),
            &[canon_root]
        ));

        fs::remove_dir_all(&root).ok();
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

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&elsewhere).ok();
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
}
