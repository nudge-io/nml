//! Schema-package resolution for the LSP (RFC 0030).
//!
//! Two separate concerns, deliberately: where package **definitions** come
//! from (workspace manifest > store `current` > builtin), and which package
//! has **binding authority** over a file (pins > unambiguous
//! auto-association > unbound fallback). Binding is exclusive — a bound
//! file's validator is built from the package's sources only, never merged
//! with the workspace scope registry — which is both what keeps strict mode
//! sound and what makes the content hash a sound validator-cache key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nml_core::ProjectConfig;
use nml_validate::package::{builtin_meta_package, DirectiveDecl, PackageError, SchemaPackage};
use nml_validate::schema::SchemaValidator;
use nml_validate::store::{Store, StoreError};

/// Where a bound package's definition came from. Ordered by determinism, which
/// is exactly the resolution precedence (RFC 0035 "delivery channels"): a
/// committed workspace manifest (in-repo) beats a provider tool's embedded
/// package (in-binary), which beats the machine-local cache (store), which
/// beats the builtin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionSource {
    /// A `<name>.package.nml` in the workspace (the authoring path; RFC 0035
    /// in-repo channel — the most deterministic source, shared via git).
    WorkspaceManifest(PathBuf),
    /// Injected in-process by an embedder that IS a schema provider — a tool
    /// (e.g. `nudge lsp`) serving the neutral server with its own embedded
    /// package (RFC 0035 in-binary channel). Beats the store so the editor
    /// validates against the exact binary in front of the user, zero-sync;
    /// yields to a committed workspace manifest, which the team chose to pin.
    InBinary,
    /// The per-user store's `current` slot (RFC 0035 in-cache channel).
    Store,
    /// Embedded in the LSP itself (today: the `package.model.nml` meta
    /// package).
    Builtin,
}

impl DefinitionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceManifest(_) => "workspace manifest",
            Self::InBinary => "in-binary",
            Self::Store => "store current",
            Self::Builtin => "builtin",
        }
    }
}

/// Which authority step bound the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingStep {
    Pinned,
    AutoAssociated,
}

/// A successful binding: everything diagnostics, hover, `nml/schemaInfo`,
/// and code actions need.
#[derive(Clone)]
pub struct Binding {
    pub package_name: String,
    pub package_version: String,
    pub content_hash: String,
    pub binding_name: String,
    pub validator: Arc<SchemaValidator>,
    pub source: DefinitionSource,
    pub step: BindingStep,
    /// The root the binding glob matched under.
    pub root: PathBuf,
    /// Set when a workspace manifest shadows a *pinned* name that the store
    /// also holds — shadowing is visible, never silent (RFC 0030).
    pub shadows_store: bool,
}

impl Binding {
    /// The single owner of the human-facing binding identity used in
    /// diagnostic suffixes and hover: `<name> blake3:<hash8>, <source>`.
    pub fn identity(&self) -> String {
        format!(
            "{} blake3:{}, {}",
            self.package_name,
            nml_validate::store::hash8(&self.content_hash),
            self.source.label()
        )
    }
}

/// One diagnostic-worthy degraded state, attached at the top of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedNote {
    pub message: String,
    pub warning: bool,
    /// Byte span into the resolved document, when the note is about a
    /// specific construct (a shadowed binding in a manifest); `None` pins
    /// the note at the top of the file.
    pub span: Option<nml_core::span::Span>,
}

/// The directive vocabulary governing a `.model.nml` file (RFC 0030
/// "Directive-vocabulary scope"): the covering package's declared
/// `[]directive` entries. `None` means the file is opaque — no covering
/// package, so directives stay syntax-checked only, with zero vocabulary
/// diagnostics (plain-nml schema authors are never punished for the
/// mechanism's existence).
pub struct VocabularyMatch {
    pub package_name: String,
    pub directives: Vec<DirectiveDecl>,
    /// Root-rule coverage only: the file sits in the covering WORKSPACE
    /// package's declared-sources directory (next to its manifest) without
    /// being in its `[]schema` — the forgot-the-manifest trap, surfaced as
    /// an info diagnostic instead of staying silent.
    pub undeclared_sibling: bool,
}

/// The answer [`PackageResolver::vocabulary_for`] gives — three-state on
/// purpose: root coverage is decided by a BOUNDED filesystem walk, and a walk
/// that hit its cap without finding a bound file is not evidence of absence.
/// Collapsing that to "opaque" would silently drop directive vocabulary on
/// large checkouts; the server owes the author an honest "undetermined".
pub enum VocabularyOutcome {
    /// A covering package's vocabulary governs the file.
    Covered(VocabularyMatch),
    /// Definitively no covering package — directives stay syntax-checked
    /// only, with zero vocabulary diagnostics (plain-nml schema authors are
    /// never punished for the mechanism's existence).
    Opaque,
    /// The coverage question could not be answered: at least one candidate
    /// package's claims walk hit its scan bound. `candidates` are the names
    /// whose coverage is unknown (a confirmed coverer is included too — with
    /// a truncated rival, even the D8 ambiguity question is open).
    Undetermined { candidates: Vec<String> },
}

/// Verdict of one bounded claims walk ([`package_claims_file_under`]).
/// `Truncated` is deliberately distinct from `NoClaim`: a capped walk proves
/// nothing, so it must be neither cached nor treated as definitive absence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimScan {
    Claims,
    NoClaim,
    Truncated,
}

/// The outcome of resolving one file.
pub enum Resolution {
    Bound(Box<Binding>),
    /// No package claims the file — today's scope-token behavior applies.
    Unbound,
}

/// Resolution result plus any degraded-state notes to surface (a file can be
/// bound *and* carry notes — e.g. a shadow info — or unbound with a note —
/// e.g. its pin's package failed to load and validation fell through).
pub struct Resolved {
    pub resolution: Resolution,
    pub notes: Vec<DegradedNote>,
}

/// A snapshot of the workspace the resolver needs for one pass; built by the
/// server from its own state so the resolver stays lock-free against server
/// internals.
pub struct WorkspaceView<'a> {
    pub roots: &'a [PathBuf],
    /// Open/indexed `<name>.package.nml` documents: (fs path, text). Open
    /// text wins over disk so manifest edits resolve live.
    pub manifests: &'a [(PathBuf, String)],
    /// Open-document lookup for schema sources named by workspace manifests —
    /// unsaved schema edits must flow into the package (the authoring path).
    pub doc_text: &'a dyn Fn(&Path) -> Option<String>,
}

/// A resolved package definition: the loaded package, its content hash, and
/// where it came from — the unit that definition precedence produces and
/// binding authority consumes.
#[derive(Clone)]
pub struct Definition {
    pub package: Arc<SchemaPackage>,
    pub hash: String,
    pub source: DefinitionSource,
}

/// Outcome of a store read, cached per pointer stat. Structured (not a
/// stringly sentinel) so degraded-state wording can stay per-variant — the
/// formatVersion contract in particular.
#[derive(Clone)]
enum StoreOutcome {
    Ready(Arc<SchemaPackage>, String),
    NotInstalled,
    /// Human-facing degraded message, already worded per the RFC contracts.
    Failed(String),
}

struct StoreCacheEntry {
    /// The pointer content at load time — the exact-by-construction
    /// freshness guard (the full hash is in it; no mtime granularity).
    pointer: Option<String>,
    outcome: StoreOutcome,
}

/// A store-package health transition (Ready↔Failed) or store-manifest
/// shadow warning, surfaced once via `window/logMessage` — never as
/// per-file diagnostics (RFC 0030: server-side conditions go to status
/// surfaces). Push-based: the resolver `try_send`s into a bounded channel
/// the server's notifier task drains the instant an event lands — no
/// per-handler drain sites, no guard-across-await hazards, and overflow
/// drops the NEWEST events (under flapping, the first transition is the
/// informative one). Send failures (full channel, dropped receiver) are
/// ignored by design: best-effort is the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEvent {
    pub message: String,
    pub warning: bool,
}

/// Fingerprint of one workspace-manifest source input, for cache
/// invalidation without re-reading content: open-document text (compared by
/// equality) or on-disk (len, mtime).
#[derive(Clone, PartialEq)]
enum SourceFingerprint {
    Doc(String),
    Disk(u64, Option<std::time::SystemTime>),
    Missing,
}

struct ManifestCacheEntry {
    manifest_text: String,
    sources: Vec<(std::path::PathBuf, SourceFingerprint)>,
    outcome: Result<(Arc<SchemaPackage>, String), String>,
}

/// Validator-cache size at which stale entries are dropped wholesale.
/// Entries rebuild on demand from cached packages; editing a workspace
/// package's schema sources mints a new content hash per keystroke, so
/// without a bound the cache grows for the length of the session.
const VALIDATOR_CACHE_CAP: usize = 64;

pub struct PackageResolver {
    store: Option<Store>,
    /// An embedder-supplied package served in-process (RFC 0035 in-binary
    /// channel): precomputed once with its content hash so resolution never
    /// re-hashes it. `None` for the neutral server; `Some` for a provider tool
    /// like `nudge lsp`. Sits above the store in precedence, below a committed
    /// workspace manifest.
    injected: Option<Definition>,
    builtin: Arc<SchemaPackage>,
    builtin_hash: String,
    store_cache: Mutex<HashMap<String, StoreCacheEntry>>,
    events: tokio::sync::mpsc::Sender<StoreEvent>,
    manifest_cache: Mutex<HashMap<std::path::PathBuf, ManifestCacheEntry>>,
    /// Validators cached per (content hash, binding name) — sound because
    /// binding is exclusive: the hash covers every input.
    validator_cache: Mutex<HashMap<(String, String), Arc<SchemaValidator>>>,
    /// Memoized [`package_claims_file_under`] answers per
    /// (package content hash, root): the walk reads up to 2048 `read_dir`
    /// entries and `vocabulary_for` runs per validation pass, so an uncached
    /// walk is a keystroke-path hazard. The content hash keys glob changes;
    /// filesystem-only changes under an unchanged package are picked up when
    /// the hash next changes (acceptable staleness for a coverage question).
    claims_cache: Mutex<HashMap<(String, PathBuf), bool>>,
}

impl PackageResolver {
    pub fn new(store: Option<Store>, events: tokio::sync::mpsc::Sender<StoreEvent>) -> Self {
        Self::with_injected(store, events, None)
    }

    /// Construct a resolver that also serves an embedder-supplied package
    /// in-process (RFC 0035 in-binary channel; the seam `nudge lsp` uses). The
    /// package's content hash is computed once here — resolution treats it like
    /// any other [`Definition`], so binding, vocabulary, caching, and the
    /// freshness poll all work unchanged.
    pub fn with_injected(
        store: Option<Store>,
        events: tokio::sync::mpsc::Sender<StoreEvent>,
        injected: Option<SchemaPackage>,
    ) -> Self {
        let builtin = Arc::new(builtin_meta_package());
        let builtin_hash = builtin.content_hash();
        let injected = injected.map(|package| {
            let hash = package.content_hash();
            Definition {
                package: Arc::new(package),
                hash,
                source: DefinitionSource::InBinary,
            }
        });
        Self {
            store,
            injected,
            builtin,
            builtin_hash,
            store_cache: Mutex::new(HashMap::new()),
            events,
            manifest_cache: Mutex::new(HashMap::new()),
            validator_cache: Mutex::new(HashMap::new()),
            claims_cache: Mutex::new(HashMap::new()),
        }
    }

    /// [`package_claims_file_under`] behind the claims cache: consult the
    /// memo for (content hash, root) first, walk only on a miss.
    fn package_claims_cached(&self, def: &Definition, root: &Path) -> ClaimScan {
        let key = (def.hash.clone(), root.to_path_buf());
        if let Some(&claims) = self
            .claims_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return if claims {
                ClaimScan::Claims
            } else {
                ClaimScan::NoClaim
            };
        }
        let scan = package_claims_file_under(&def.package, root);
        // Only DEFINITIVE verdicts are cached: a truncated walk proves
        // nothing, and memoizing it would freeze "undetermined" into a wrong
        // yes/no until the content hash next changes.
        if let verdict @ (ClaimScan::Claims | ClaimScan::NoClaim) = scan {
            let mut cache = self.claims_cache.lock().unwrap_or_else(|e| e.into_inner());
            // Bounded like the validator cache and for the same reason: source
            // edits mint a new content hash per keystroke, so unbounded entries
            // grow for the length of the session. Cap-and-clear; walks are cheap
            // to redo on demand.
            if cache.len() >= VALIDATOR_CACHE_CAP {
                cache.clear();
            }
            cache.insert(key, verdict == ClaimScan::Claims);
        }
        scan
    }

    /// Invalidate cached coverage verdicts. Called on watched-file
    /// create/delete: a claim is a statement about which files exist under a
    /// root, and an unchanged package's hash never changes — without this,
    /// a moved or deleted bound file leaves a stale verdict (wrong directive
    /// squiggles, or vocabulary silently off) until restart.
    pub fn invalidate_claims(&self) {
        self.claims_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Resolve one file. `path` must be absolute.
    pub fn resolve(&self, path: &Path, ws: &WorkspaceView<'_>) -> Resolved {
        let mut notes = Vec::new();

        // Per-root project settings: nearest-ancestor nml-project.nml wins
        // wholesale; lists never merge across nesting levels (RFC 0030).
        let project = nearest_project_config(path, ws);
        let (pins, auto_associate) = match &project {
            // `pinned_packages()` folds in the `provider` tool as an implicit
            // same-named pin (RFC 0035 tool→package fallback), so the neutral
            // server validates a provider-declared project against the tool's
            // published package without launching the tool's LSP. Each pin is
            // charset-gated in the loop below.
            Some((_, config)) => (config.pinned_packages(), config.auto_associate),
            None => (Vec::new(), true),
        };

        // ── Step 1: pins, in list order, first match wins. ──
        for pin in &pins {
            // A pin is an external string headed for store paths and
            // diagnostics; the charset rule guards every resolution path
            // (RFC 0030 Security) — a hostile `../../x` pin is rejected
            // here, never joined into a path or echoed into a "run this"
            // hint.
            if !nml_validate::package::valid_package_name(pin) {
                notes.push(DegradedNote {
                    message: format!(
                        "schema package name {pin:?} is not a valid package name ([a-z][a-z0-9-]*) — ignored (from schemaPackages or provider.tool)"
                    ),
                    warning: true,
                    span: None,
                });
                continue;
            }
            match self.definition_for(pin, path, ws, &mut notes) {
                Some(def) => {
                    if let Some(binding) =
                        self.try_bind(&def, path, ws, BindingStep::Pinned, &mut notes)
                    {
                        let shadows_store =
                            matches!(def.source, DefinitionSource::WorkspaceManifest(_))
                                && self.store_has(pin);
                        if shadows_store {
                            notes.push(DegradedNote {
                                message: format!(
                                    "bound by workspace manifest for '{pin}', shadowing the store's copy"
                                ),
                                warning: false,
                                span: None,
                            });
                        }
                        return Resolved {
                            resolution: Resolution::Bound(Box::new(Binding {
                                shadows_store,
                                ..binding
                            })),
                            notes,
                        };
                    }
                }
                None => {
                    // definition_for pushed the precise note (not installed /
                    // failed to load); the pin simply doesn't bind.
                }
            }
        }

        // ── Step 2: unambiguous auto-association across known packages. ──
        if auto_associate {
            let mut matches: Vec<Binding> = Vec::new();
            for def in self.known_packages(path, ws, &mut notes) {
                if let Some(binding) =
                    self.try_bind(&def, path, ws, BindingStep::AutoAssociated, &mut notes)
                {
                    matches.push(binding);
                }
            }
            match matches.len() {
                1 => {
                    let binding = matches.into_iter().next().expect("len checked");
                    return Resolved {
                        resolution: Resolution::Bound(Box::new(binding)),
                        notes,
                    };
                }
                0 => {}
                _ => {
                    let names: Vec<&str> =
                        matches.iter().map(|b| b.package_name.as_str()).collect();
                    notes.push(DegradedNote {
                        message: format!(
                            "{} schema packages claim this file ({}) — add a schemaPackages pin to choose",
                            names.len(),
                            names.join(", ")
                        ),
                        warning: true,
                        span: None,
                    });
                }
            }
        }

        Resolved {
            resolution: Resolution::Unbound,
            notes,
        }
    }

    /// The directive vocabulary covering `path` (RFC 0030): (a) a governing
    /// workspace manifest whose `[]schema` declares this exact file wins;
    /// else (b) the unique known package — deepest manifest first, then
    /// store; never the builtin — binding at least one file under this
    /// path's root; else `None` (opaque). The builtin is excluded by the spec's own enumeration: it
    /// governs `*.package.nml` manifests, not model sources, and its empty
    /// vocabulary would turn every directive in an operator repo into an
    /// unknown-name error merely because a manifest file exists somewhere
    /// under the root.
    pub fn vocabulary_for(&self, path: &Path, ws: &WorkspaceView<'_>) -> VocabularyOutcome {
        // (a) Declared source: the authoring path. Probing must stay quiet
        // (same rule as auto-association), so load notes are discarded here —
        // a failing manifest's own diagnostics surface when *it* is resolved.
        let mut quiet = Vec::new();
        for (manifest_path, text) in Self::manifests_governing(path, ws) {
            let Some(dir) = manifest_path.parent() else {
                continue;
            };
            if let Some((package, _)) =
                self.load_workspace_manifest(manifest_path, text, ws, &mut quiet)
            {
                if package
                    .manifest
                    .schemas
                    .iter()
                    .any(|entry| dir.join(&entry.file) == path)
                {
                    return VocabularyOutcome::Covered(VocabularyMatch {
                        package_name: package.manifest.name.clone(),
                        directives: package.manifest.directives.clone(),
                        undeclared_sibling: false,
                    });
                }
            }
        }

        // (b) Root-level coverage: a root "has" a package when at least one
        // file under it is bound to that package. Multiple covering packages
        // is v1 future work (Decision log D8) — ambiguity degrades to opaque
        // rather than guessing a vocabulary. A TRUNCATED walk answers
        // nothing, so it degrades to `Undetermined`, never to a silent
        // opaque.
        let mut covering: Vec<VocabularyMatch> = Vec::new();
        let mut truncated: Vec<String> = Vec::new();
        for def in self.known_packages(path, ws, &mut quiet) {
            if matches!(def.source, DefinitionSource::Builtin) {
                continue;
            }
            let Some(root) = find_root(path, &def.package.manifest.root_markers, ws, &def.source)
            else {
                continue;
            };
            match self.package_claims_cached(&def, &root) {
                ClaimScan::Claims => {
                    let undeclared_sibling = matches!(
                        &def.source,
                        DefinitionSource::WorkspaceManifest(mp)
                            if mp.parent() == path.parent()
                    );
                    covering.push(VocabularyMatch {
                        package_name: def.package.manifest.name.clone(),
                        directives: def.package.manifest.directives.clone(),
                        undeclared_sibling,
                    });
                    if covering.len() == 2 {
                        // Two coverers already means ambiguity ⇒ opaque (D8);
                        // walking the remaining packages cannot change that,
                        // truncated candidates included.
                        return VocabularyOutcome::Opaque;
                    }
                }
                ClaimScan::NoClaim => {}
                ClaimScan::Truncated => {
                    truncated.push(def.package.manifest.name.clone());
                }
            }
        }
        match (covering.len(), truncated.is_empty()) {
            (1, true) => VocabularyOutcome::Covered(covering.pop().expect("len checked")),
            (_, true) => VocabularyOutcome::Opaque,
            // Any truncation leaves the question open: a truncated candidate
            // might have covered (or made a confirmed coverer ambiguous), so
            // every unresolved name — confirmed coverer included — is a
            // candidate.
            _ => VocabularyOutcome::Undetermined {
                candidates: covering
                    .iter()
                    .map(|c| c.package_name.clone())
                    .chain(truncated)
                    .collect(),
            },
        }
    }

    /// Workspace manifests governing `path` — a manifest defines its package
    /// for files under its own directory subtree only (RFC 0030: "each
    /// governs its root"; a manifest buried in a vendored dir or fixture
    /// must not redefine validation workspace-wide). Deepest-first so the
    /// nearest manifest wins for nesting; deduped by name downstream.
    fn manifests_governing<'m>(
        path: &Path,
        ws: &'m WorkspaceView<'_>,
    ) -> Vec<&'m (PathBuf, String)> {
        let mut governing: Vec<&(PathBuf, String)> = ws
            .manifests
            .iter()
            .filter(|(mp, _)| mp.parent().is_some_and(|dir| path.starts_with(dir)))
            .collect();
        governing.sort_by_key(|(mp, _)| std::cmp::Reverse(mp.components().count()));
        governing
    }

    /// All known packages *for this file*: governing workspace manifests,
    /// store entries, builtins — workspace definitions shadow same-named
    /// store entries; nearest manifest shadows a farther same-named one.
    ///
    /// Load failures during this probing pass are deliberately quiet except
    /// on the failing manifest itself: an unloadable package's globs are
    /// unknowable, so the "files it would have bound" contract cannot be
    /// met, and broadcasting the failure onto every resolved file in the
    /// workspace is worse. Pinned resolution (which names the package
    /// explicitly) stays loud.
    fn known_packages(
        &self,
        path: &Path,
        ws: &WorkspaceView<'_>,
        notes: &mut Vec<DegradedNote>,
    ) -> Vec<Definition> {
        let mut out: Vec<Definition> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (manifest_path, text) in Self::manifests_governing(path, ws) {
            let is_self = path == manifest_path;
            let mut local = Vec::new();
            if let Some((package, hash)) =
                self.load_workspace_manifest(manifest_path, text, ws, &mut local)
            {
                // Shadow warnings belong to THIS manifest only — spans are
                // byte offsets into THIS text; reading them off any other
                // definition would squiggle arbitrary bytes.
                if is_self {
                    for w in package.manifest.shadow_warnings() {
                        notes.push(DegradedNote {
                            message: w.message,
                            warning: true,
                            span: w.span,
                        });
                    }
                }
                if seen.contains(&package.manifest.name) {
                    continue; // nearer manifest already defines this name
                }
                seen.push(package.manifest.name.clone());
                out.push(Definition {
                    package,
                    hash,
                    source: DefinitionSource::WorkspaceManifest(manifest_path.clone()),
                });
            }
            if is_self {
                notes.extend(local);
            }
        }
        // In-binary channel (RFC 0035): above the store, below a committed
        // workspace manifest of the same name — the team's committed copy wins,
        // but the embedded package always beats its own possibly-stale cache.
        if let Some(def) = &self.injected {
            if !seen.contains(&def.package.manifest.name) {
                seen.push(def.package.manifest.name.clone());
                out.push(def.clone());
            }
        }
        if let Some(store) = &self.store {
            for name in store.list_names() {
                if seen.contains(&name) {
                    continue;
                }
                if let StoreOutcome::Ready(package, hash) = self.load_store_package(&name) {
                    out.push(Definition {
                        package,
                        hash,
                        source: DefinitionSource::Store,
                    });
                }
            }
        }
        if !seen.contains(&self.builtin.manifest.name) {
            out.push(Definition {
                package: self.builtin.clone(),
                hash: self.builtin_hash.clone(),
                source: DefinitionSource::Builtin,
            });
        }
        out
    }

    /// Definition precedence for one named package, scoped to the file's
    /// governing manifests: workspace manifest > store `current` > builtin.
    fn definition_for(
        &self,
        name: &str,
        path: &Path,
        ws: &WorkspaceView<'_>,
        notes: &mut Vec<DegradedNote>,
    ) -> Option<Definition> {
        for (manifest_path, text) in Self::manifests_governing(path, ws) {
            if manifest_stem(manifest_path) == Some(name) {
                if let Some((package, hash)) =
                    self.load_workspace_manifest(manifest_path, text, ws, notes)
                {
                    return Some(Definition {
                        package,
                        hash,
                        source: DefinitionSource::WorkspaceManifest(manifest_path.clone()),
                    });
                }
                return None;
            }
        }
        // In-binary channel (RFC 0035): a pin resolves to the embedded package
        // before the store, so `nudge lsp` validates against the running
        // binary's schema even when the store holds an older synced copy.
        if let Some(def) = &self.injected {
            if def.package.manifest.name == name {
                return Some(def.clone());
            }
        }
        match self.load_store_package(name) {
            StoreOutcome::Ready(package, hash) => {
                return Some(Definition {
                    package,
                    hash,
                    source: DefinitionSource::Store,
                })
            }
            StoreOutcome::Failed(message) => {
                // The pin names this package explicitly — its failure is
                // this file's business.
                notes.push(DegradedNote {
                    message,
                    warning: true,
                    span: None,
                });
                return None;
            }
            StoreOutcome::NotInstalled => {}
        }
        if self.builtin.manifest.name == name {
            return Some(Definition {
                package: self.builtin.clone(),
                hash: self.builtin_hash.clone(),
                source: DefinitionSource::Builtin,
            });
        }
        if self.store.is_some() {
            notes.push(DegradedNote {
                message: format!(
                    "pinned schema package '{name}' is not installed — run '{name} schema sync'"
                ),
                warning: true,
                span: None,
            });
        }
        None
    }

    /// The store this resolver binds against — for the freshness poll
    /// (RFC 0030), which must watch the SAME store resolution reads. The
    /// harness injects a tempdir store through `with_store`; a poll built on
    /// `Store::user()` would watch the wrong directory there and in any
    /// embedder with a custom store.
    pub(crate) fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    fn store_has(&self, name: &str) -> bool {
        self.store
            .as_ref()
            .is_some_and(|s| s.pointer_content(name).is_some())
    }

    /// Store read with a stat-guarded cache: the per-validation-pass probe is
    /// a `stat` (microseconds); the package is re-read and re-hashed only on
    /// a pointer transition — including absent→present, the
    /// brand-new-operator path (RFC 0030 Freshness). Failure wording is
    /// per-variant here so the formatVersion degradation contract survives
    /// the store path (its primary path — a newer nudge auto-syncing).
    fn load_store_package(&self, name: &str) -> StoreOutcome {
        let Some(store) = self.store.as_ref() else {
            return StoreOutcome::NotInstalled;
        };
        // One ~80-byte read serves as both freshness guard and load input.
        let pointer = store.pointer_content(name);
        let mut cache = self.store_cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(name) {
            Some(e) if e.pointer == pointer => e.outcome.clone(),
            prior => {
                let outcome = match &pointer {
                    None => StoreOutcome::NotInstalled,
                    Some(content) => match store.load_current(name, content) {
                        Ok(slot) => {
                            // A store manifest isn't an open file — its
                            // shadow warnings go to the status channel,
                            // one-shot by construction (loads happen once
                            // per pointer transition).
                            // One-shot by cache-miss construction; may drop
                            // on overflow (see StoreEvent: newest-dropped).
                            for w in slot.package.manifest.shadow_warnings() {
                                let _ = self.events.try_send(StoreEvent {
                                    message: format!(
                                        "schema package '{name}': {}",
                                        w.message
                                    ),
                                    warning: true,
                                });
                            }
                            StoreOutcome::Ready(Arc::new(slot.package), slot.content_hash)
                        }
                        Err(StoreError::NotInstalled) => StoreOutcome::NotInstalled,
                        Err(StoreError::Package(
                            nml_validate::package::PackageError::UnsupportedFormatVersion {
                                required,
                                supported,
                            },
                        )) => StoreOutcome::Failed(format!(
                            "schema package '{name}' needs formatVersion {required}; this nml-lsp supports {supported} — update nml-lsp; using basic validation until then"
                        )),
                        Err(e) => StoreOutcome::Failed(format!(
                            "schema package '{name}' in the store failed to load: {e} — falling back to basic validation"
                        )),
                    },
                };
                // Health transitions surface once, at the transition.
                let was_failed = matches!(prior.map(|e| &e.outcome), Some(StoreOutcome::Failed(_)));
                match (&outcome, was_failed) {
                    (StoreOutcome::Failed(message), false) => {
                        let _ = self.events.try_send(StoreEvent {
                            message: message.clone(),
                            warning: true,
                        });
                    }
                    (StoreOutcome::Ready(..), true) => {
                        let _ = self.events.try_send(StoreEvent {
                            message: format!("schema package '{name}' in the store recovered"),
                            warning: false,
                        });
                    }
                    _ => {}
                }
                cache.insert(
                    name.to_string(),
                    StoreCacheEntry {
                        pointer,
                        outcome: outcome.clone(),
                    },
                );
                outcome
            }
        }
    }

    /// Load a workspace manifest package: manifest text from the live
    /// document, sources from open documents first (unsaved edits flow into
    /// the package — the authoring loop), disk second. Fingerprint-cached so
    /// the steady-state per-pass cost is string compares + stats, never
    /// re-parse/re-hash (RFC 0030 Freshness: "hash verification on
    /// load/cache-miss only, never per keystroke").
    fn load_workspace_manifest(
        &self,
        manifest_path: &Path,
        text: &str,
        ws: &WorkspaceView<'_>,
        notes: &mut Vec<DegradedNote>,
    ) -> Option<(Arc<SchemaPackage>, String)> {
        let fingerprint_of = |full: &Path| -> SourceFingerprint {
            if let Some(open) = (ws.doc_text)(full) {
                return SourceFingerprint::Doc(open);
            }
            match std::fs::metadata(full) {
                Ok(m) => SourceFingerprint::Disk(m.len(), m.modified().ok()),
                Err(_) => SourceFingerprint::Missing,
            }
        };
        {
            let cache = self
                .manifest_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(manifest_path) {
                if entry.manifest_text == text
                    && entry.sources.iter().all(|(p, fp)| fingerprint_of(p) == *fp)
                {
                    match &entry.outcome {
                        Ok((package, hash)) => return Some((package.clone(), hash.clone())),
                        Err(message) => {
                            notes.push(DegradedNote {
                                message: message.clone(),
                                warning: true,
                                span: None,
                            });
                            return None;
                        }
                    }
                }
            }
        }

        let dir = manifest_path.parent()?;
        let mut fingerprints: Vec<(PathBuf, SourceFingerprint)> = Vec::new();
        let result = SchemaPackage::from_parts(text, |file| {
            nml_validate::package::check_plain_file_name(file)?;
            let full = dir.join(file);
            let fp = fingerprint_of(&full);
            let content = match &fp {
                SourceFingerprint::Doc(open) => Ok(open.clone()),
                _ => std::fs::read_to_string(&full).map_err(|e| e.to_string()),
            };
            fingerprints.push((full, fp));
            content
        });
        let outcome: Result<(Arc<SchemaPackage>, String), String> = match result {
            Ok(package) => {
                // The filename stem is the pin/dedup key; the declared name
                // is the binding identity. They must agree — the store
                // enforces this, and a `demo.package.nml` declaring
                // `package nudge:` must not be two different packages
                // depending on the resolution path.
                match manifest_stem(manifest_path) {
                    Some(stem) if stem != package.manifest.name => Err(format!(
                        "workspace package manifest '{}' declares package '{}' but its filename says '{stem}' — rename one; using basic validation until then",
                        manifest_path.display(),
                        package.manifest.name
                    )),
                    _ => {
                        let hash = package.content_hash();
                        Ok((Arc::new(package), hash))
                    }
                }
            }
            Err(PackageError::UnsupportedFormatVersion {
                required,
                supported,
            }) => Err(format!(
                "package manifest '{}' needs formatVersion {required}; this nml-lsp supports {supported} — update nml-lsp; using basic validation until then",
                manifest_path.display()
            )),
            Err(e) => Err(format!(
                "workspace package manifest '{}' failed to load: {e} — falling back to basic validation",
                manifest_path.display()
            )),
        };

        self.manifest_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                manifest_path.to_path_buf(),
                ManifestCacheEntry {
                    manifest_text: text.to_string(),
                    sources: fingerprints,
                    outcome: outcome.clone(),
                },
            );
        match outcome {
            Ok((package, hash)) => Some((package, hash)),
            Err(message) => {
                notes.push(DegradedNote {
                    message,
                    warning: true,
                    span: None,
                });
                None
            }
        }
    }

    /// Try to bind `path` with one package: find its root, match the binding
    /// globs, and build (or fetch) the exclusive validator.
    fn try_bind(
        &self,
        def: &Definition,
        path: &Path,
        ws: &WorkspaceView<'_>,
        step: BindingStep,
        notes: &mut Vec<DegradedNote>,
    ) -> Option<Binding> {
        let Definition {
            package,
            hash,
            source,
        } = def;
        let root = find_root(path, &package.manifest.root_markers, ws, source)?;
        let rel = path.strip_prefix(&root).ok()?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let binding = package.binding_for(&rel)?;

        let key = (hash.to_string(), binding.name.clone());
        let cached = {
            let cache = self
                .validator_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache.get(&key).cloned()
        };
        let validator = match cached {
            Some(v) => v,
            None => match package.validator(binding) {
                Ok(v) => {
                    let v = Arc::new(v);
                    let mut cache = self
                        .validator_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    // Bounded: schema-source edits mint a fresh hash per
                    // keystroke; entries rebuild cheaply on demand, so a
                    // wholesale clear at the cap beats bookkeeping.
                    if cache.len() >= VALIDATOR_CACHE_CAP {
                        cache.clear();
                    }
                    cache.insert(key, v.clone());
                    v
                }
                Err(PackageError::Sources { errors }) => {
                    let detail = errors
                        .first()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "unknown error".to_string());
                    notes.push(DegradedNote {
                        message: format!(
                            "schema package '{}' failed to load: {detail} — falling back to basic validation",
                            package.manifest.name
                        ),
                        warning: true,
                        span: None,
                    });
                    return None;
                }
                Err(e) => {
                    notes.push(DegradedNote {
                        message: format!(
                            "schema package '{}' failed to load: {e} — falling back to basic validation",
                            package.manifest.name
                        ),
                        warning: true,
                        span: None,
                    });
                    return None;
                }
            },
        };

        Some(Binding {
            package_name: package.manifest.name.clone(),
            package_version: package.manifest.version.clone(),
            content_hash: hash.to_string(),
            binding_name: binding.name.clone(),
            validator,
            source: source.clone(),
            step,
            root,
            // The pinned caller overlays this after its store check; it has
            // no meaning on other paths.
            shadows_store: false,
        })
    }
}

/// Does `package` bind at least one file under `root`? Bounded filesystem
/// walk (depth- and count-capped, hidden/`node_modules`/`.git` skipped) — the
/// vocabulary question is per-root, not per-file, and roots can be large
/// checkouts; an unbounded walk on every validation pass of a model file
/// would be a keystroke-path hazard. First bound match wins, so the common
/// case (a marker file like `demo.nml` sitting directly in the root) exits
/// after a handful of entries.
fn package_claims_file_under(package: &SchemaPackage, root: &Path) -> ClaimScan {
    const MAX_DEPTH: usize = 12;
    const MAX_ENTRIES: usize = 2048;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut visited = 0usize;
    // Set when a bound (depth or entry cap) cut the walk short: "no bound
    // file SEEN" is then not "no bound file EXISTS", and the caller must not
    // treat it as definitive absence.
    let mut truncated = false;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES {
                return ClaimScan::Truncated;
            }
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                // Hidden/`node_modules`/`target` skips are POLICY (those
                // trees are never claimable), not truncation; only the
                // depth cap cuts off directories the walk wanted to see.
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                } else {
                    truncated = true;
                }
                continue;
            }
            if !name.ends_with(".nml") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if package.binding_for(&rel).is_some() {
                return ClaimScan::Claims;
            }
        }
    }
    if truncated {
        ClaimScan::Truncated
    } else {
        ClaimScan::NoClaim
    }
}

fn manifest_stem(path: &Path) -> Option<&str> {
    path.file_name()?
        .to_str()?
        .strip_suffix(".package.nml")
        .filter(|s| !s.is_empty())
}

/// The nearest-ancestor `nml-project.nml` from `path`'s directory upward,
/// bounded by the workspace roots. Nearest file wins wholesale.
pub fn nearest_project_config(
    path: &Path,
    ws: &WorkspaceView<'_>,
) -> Option<(PathBuf, ProjectConfig)> {
    for dir in ancestors_within_roots(path, ws.roots) {
        let candidate = dir.join("nml-project.nml");
        let text = (ws.doc_text)(&candidate).or_else(|| std::fs::read_to_string(&candidate).ok());
        if let Some(text) = text {
            let file = nml_core::cst::parse_best_effort(&text);
            return Some((candidate, ProjectConfig::from_file(&file)));
        }
    }
    None
}

/// The project root for glob anchoring (RFC 0030 Vocabulary): nearest
/// ancestor (inclusive) containing `nml-project.nml` or one of the package's
/// root markers; the workspace-manifest's own directory is the root of last
/// resort, which makes the rule total.
fn find_root(
    path: &Path,
    root_markers: &[String],
    ws: &WorkspaceView<'_>,
    source: &DefinitionSource,
) -> Option<PathBuf> {
    for dir in ancestors_within_roots(path, ws.roots) {
        if dir.join("nml-project.nml").is_file()
            || root_markers.iter().any(|m| dir.join(m).is_file())
        {
            return Some(dir);
        }
    }
    match source {
        DefinitionSource::WorkspaceManifest(manifest_path) => {
            manifest_path.parent().map(Path::to_path_buf)
        }
        // In-binary/store/builtin packages with no marker root: the workspace
        // root containing the file anchors the globs. Enumerated (not a
        // catch-all) so a new source with different anchoring can't fall
        // through silently.
        DefinitionSource::InBinary | DefinitionSource::Store | DefinitionSource::Builtin => ws
            .roots
            .iter()
            .find(|r| path.starts_with(r))
            .cloned()
            .or_else(|| path.parent().map(Path::to_path_buf)),
    }
}

/// Directories from the file's parent upward, stopping at (and including)
/// the containing workspace root; outside any root, just the parent chain
/// bounded to a sane depth.
fn ancestors_within_roots(path: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let containing_root = roots.iter().find(|r| path.starts_with(r));
    let mut dir = path.parent();
    let mut depth = 0;
    while let Some(d) = dir {
        out.push(d.to_path_buf());
        if let Some(root) = containing_root {
            if d == root.as_path() {
                break;
            }
        }
        depth += 1;
        if depth >= 64 {
            break;
        }
        dir = d.parent();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use nml_validate::test_support::{publish_demo, DEMO_CORE as CORE, DEMO_MANIFEST as MANIFEST};

    fn temp_ws(tag: &str) -> PathBuf {
        // pid + process-wide counter: pid alone collides when a re-used pid
        // (or a same-process re-entry) hits the same tag.
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nml-pkg-test-{tag}-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_docs(_: &Path) -> Option<String> {
        None
    }

    /// Unwrap a `Covered` outcome; panics (with the reason) otherwise.
    fn covered(outcome: VocabularyOutcome, why: &str) -> VocabularyMatch {
        match outcome {
            VocabularyOutcome::Covered(m) => m,
            VocabularyOutcome::Opaque => panic!("expected coverage ({why}), got Opaque"),
            VocabularyOutcome::Undetermined { candidates } => {
                panic!("expected coverage ({why}), got Undetermined({candidates:?})")
            }
        }
    }

    /// Event channel for tests: keep the receiver alive via a leak-free
    /// return; most tests drop it (send failures are the contract's
    /// no-listener case).
    fn test_events() -> (
        tokio::sync::mpsc::Sender<StoreEvent>,
        tokio::sync::mpsc::Receiver<StoreEvent>,
    ) {
        tokio::sync::mpsc::channel(64)
    }

    #[test]
    fn auto_association_binds_store_package_by_marker_root() {
        let ws = temp_ws("auto");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base));
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site")).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();

        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        // Marker file at proj/ anchors the glob: both files bind.
        for rel in ["demo.nml", "apps/site/app.nml"] {
            let resolved = resolver.resolve(&project.join(rel), &view);
            match resolved.resolution {
                Resolution::Bound(b) => {
                    assert_eq!(b.package_name, "demo");
                    assert_eq!(b.step, BindingStep::AutoAssociated);
                    assert_eq!(b.source, DefinitionSource::Store);
                    assert_eq!(b.root, project);
                }
                Resolution::Unbound => panic!("{rel} should bind"),
            }
        }
        // A file outside the globs stays unbound.
        std::fs::write(project.join("other.nml"), "").unwrap();
        assert!(matches!(
            resolver
                .resolve(&project.join("other.nml"), &view)
                .resolution,
            Resolution::Unbound
        ));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A `demo` package with a distinguishing version, so precedence tests can
    /// tell an injected/store/workspace copy of the same name apart by
    /// `package_version` as well as by `source`.
    fn demo_package_versioned(version: &str) -> SchemaPackage {
        let manifest = MANIFEST.replace("version = \"0.1.0\"", &format!("version = \"{version}\""));
        SchemaPackage::from_parts(&manifest, |_| Ok(CORE.to_string())).expect("demo package loads")
    }

    /// RFC 0035 in-binary channel: an injected package binds a file with NO
    /// store and NO workspace manifest — the `nudge lsp` zero-install path.
    #[test]
    fn injected_package_binds_with_no_store_no_manifest() {
        let ws = temp_ws("inj-alone");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();

        let resolver = PackageResolver::with_injected(
            None,
            test_events().0,
            Some(demo_package_versioned("9.9.9")),
        );
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        match resolver
            .resolve(&project.join("demo.nml"), &view)
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(b.package_name, "demo");
                assert_eq!(b.source, DefinitionSource::InBinary);
                assert_eq!(b.package_version, "9.9.9");
                assert_eq!(b.root, project);
            }
            Resolution::Unbound => panic!("injected package should bind demo.nml"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Determinism ladder (RFC 0035): the in-binary package beats its own
    /// possibly-stale cache. Store holds `demo` 0.1.0; the injected `demo`
    /// 9.9.9 wins — zero-sync coherence.
    #[test]
    fn injected_beats_store_for_same_name() {
        let ws = temp_ws("inj-store");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base)); // demo 0.1.0
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();

        let resolver = PackageResolver::with_injected(
            Some(Store::at(&store_base)),
            test_events().0,
            Some(demo_package_versioned("9.9.9")),
        );
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        match resolver
            .resolve(&project.join("demo.nml"), &view)
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(
                    b.source,
                    DefinitionSource::InBinary,
                    "in-binary beats cache"
                );
                assert_eq!(b.package_version, "9.9.9");
            }
            Resolution::Unbound => panic!("should bind"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Determinism ladder (RFC 0035): a committed workspace manifest — the
    /// team's chosen source of truth — beats the in-binary package.
    #[test]
    fn workspace_manifest_beats_injected_for_same_name() {
        let ws = temp_ws("inj-ws");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        let manifest_path = project.join("demo.package.nml");
        // Committed manifest for `demo` at version 2.0.0; its declared source
        // `core.model.nml` is read from disk (doc_text is empty here).
        let manifest_text = MANIFEST.replace("version = \"0.1.0\"", "version = \"2.0.0\"");
        std::fs::write(&manifest_path, &manifest_text).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();

        let resolver = PackageResolver::with_injected(
            None,
            test_events().0,
            Some(demo_package_versioned("9.9.9")),
        );
        let roots = vec![ws.clone()];
        let manifests = vec![(manifest_path.clone(), manifest_text.clone())];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        match resolver
            .resolve(&project.join("demo.nml"), &view)
            .resolution
        {
            Resolution::Bound(b) => {
                assert!(
                    matches!(b.source, DefinitionSource::WorkspaceManifest(_)),
                    "committed manifest beats in-binary, got {:?}",
                    b.source
                );
                assert_eq!(b.package_version, "2.0.0");
            }
            Resolution::Unbound => panic!("should bind"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn opt_out_disables_auto_association_and_pin_restores() {
        let ws = temp_ws("optout");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base));
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    autoAssociate = false\n",
        )
        .unwrap();

        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        assert!(matches!(
            resolver
                .resolve(&project.join("demo.nml"), &view)
                .resolution,
            Resolution::Unbound
        ));

        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    autoAssociate = false\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        match resolver
            .resolve(&project.join("demo.nml"), &view)
            .resolution
        {
            Resolution::Bound(b) => assert_eq!(b.step, BindingStep::Pinned),
            Resolution::Unbound => panic!("pin must bind"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn workspace_manifest_shadows_store_with_visible_note() {
        let ws = temp_ws("shadow");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base));
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();

        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let manifests = vec![(project.join("demo.package.nml"), MANIFEST.to_string())];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        let resolved = resolver.resolve(&project.join("demo.nml"), &view);
        match resolved.resolution {
            Resolution::Bound(b) => {
                assert!(matches!(b.source, DefinitionSource::WorkspaceManifest(_)));
                assert!(b.shadows_store);
            }
            Resolution::Unbound => panic!("must bind"),
        }
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("shadowing")),
            "{:?}",
            resolved.notes
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn missing_pin_notes_and_falls_through() {
        let ws = temp_ws("missingpin");
        let store_base = ws.join("store");
        std::fs::create_dir_all(store_base.join("schema-packages")).unwrap();
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("whatever.nml"), "").unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - ghost\n",
        )
        .unwrap();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let resolved = resolver.resolve(&project.join("whatever.nml"), &view);
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("'ghost' is not installed")),
            "{:?}",
            resolved.notes
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// M1 (security review): a hostile pin never reaches a store path or a
    /// remediation hint — rejected with a note, resolution proceeds.
    #[test]
    fn hostile_pin_names_are_rejected() {
        let ws = temp_ws("hostilepin");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("x.nml"), "").unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - \"../../etc\"\n",
        )
        .unwrap();
        let resolver = PackageResolver::new(Some(Store::at(ws.join("store"))), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let resolved = resolver.resolve(&project.join("x.nml"), &view);
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("not a valid package name")),
            "{:?}",
            resolved.notes
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// M2 (security review): a manifest governs only files under its own
    /// directory subtree — a manifest buried in a sibling dir must not
    /// define packages for the rest of the workspace.
    #[test]
    fn manifest_governs_only_its_subtree() {
        let ws = temp_ws("subtree");
        let vendored = ws.join("vendored");
        let project = ws.join("proj");
        std::fs::create_dir_all(&vendored).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(vendored.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(vendored.join("core.model.nml"), CORE).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let manifests = vec![(vendored.join("demo.package.nml"), MANIFEST.to_string())];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        // proj/demo.nml is outside vendored/ — the manifest must not claim it.
        assert!(matches!(
            resolver
                .resolve(&project.join("demo.nml"), &view)
                .resolution,
            Resolution::Unbound
        ));
        // …but a file inside the manifest's subtree binds.
        std::fs::write(vendored.join("demo.nml"), "").unwrap();
        assert!(matches!(
            resolver
                .resolve(&vendored.join("demo.nml"), &view)
                .resolution,
            Resolution::Bound(_)
        ));
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// M5 (both reviews): filename stem and declared package name must
    /// agree, mirroring the store check — otherwise one file is two
    /// different packages depending on the resolution path.
    #[test]
    fn stem_name_mismatch_is_rejected_with_note() {
        let ws = temp_ws("stem");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let mismatched = MANIFEST.replace("package demo:", "package other:");
        let manifest_path = project.join("demo.package.nml");
        std::fs::write(&manifest_path, &mismatched).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let manifests = vec![(manifest_path.clone(), mismatched.clone())];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        // Resolving the manifest itself surfaces the mismatch note (it still
        // binds to the builtin meta package for validation).
        let resolved = resolver.resolve(&manifest_path, &view);
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("its filename says 'demo'")),
            "{:?}",
            resolved.notes
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// M4 (both reviews): the formatVersion degradation contract holds on
    /// the store path — the wording names the component and the number.
    #[test]
    fn store_format_version_gate_uses_contract_wording() {
        let ws = temp_ws("storefv");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        let future = MANIFEST.replace("formatVersion = 1", "formatVersion = 99");
        // Hand-write the slot: from_parts would reject the manifest here.
        let slot_dir = store_base.join("schema-packages/demo/0.1.0+deadbeef");
        std::fs::create_dir_all(&slot_dir).unwrap();
        std::fs::write(slot_dir.join("demo.package.nml"), &future).unwrap();
        std::fs::write(slot_dir.join("core.model.nml"), CORE).unwrap();
        std::fs::write(
            store_base.join("schema-packages/demo/current"),
            "0.1.0+deadbeef\nblake3:doesnotmatter\n",
        )
        .unwrap();
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("x.nml"), "").unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let resolved = resolver.resolve(&project.join("x.nml"), &view);
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("needs formatVersion 99")
                    && n.message.contains("update nml-lsp")),
            "{:?}",
            resolved.notes
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// M4/M3 (both reviews): a broken store package must not spam notes onto
    /// files it never claimed — auto-association probing is quiet.
    #[test]
    fn broken_store_package_is_quiet_for_unpinned_files() {
        let ws = temp_ws("quietcorrupt");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base));
        // Corrupt the slot after writing.
        let pkg_dir = store_base.join("schema-packages/demo");
        std::fs::write(pkg_dir.join("current"), "0.1.0+badbadba\nblake3:wrong\n").unwrap();
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("unrelated.nml"), "").unwrap();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let resolved = resolver.resolve(&project.join("unrelated.nml"), &view);
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Push-based events: a store failure transition arrives on the channel
    /// the instant the resolve that discovered it runs — no drains anywhere.
    #[test]
    fn store_failure_transition_is_pushed_to_the_channel() {
        let ws = temp_ws("eventpush");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(&store_base));
        let pkg_dir = store_base.join("schema-packages/demo");
        std::fs::write(pkg_dir.join("current"), "0.1.0+bad00000\nblake3:wrong\n").unwrap();
        let (tx, mut rx) = test_events();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), tx);
        std::fs::create_dir_all(ws.join("proj")).unwrap();
        std::fs::write(
            ws.join("proj/nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        std::fs::write(ws.join("proj/x.nml"), "").unwrap();
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let _ = resolver.resolve(&ws.join("proj/x.nml"), &view);
        let ev = rx.try_recv().expect("failure transition pushed");
        assert!(
            ev.warning && ev.message.contains("failed to load"),
            "{ev:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "one-shot: no duplicate on same state"
        );
        let _ = resolver.resolve(&ws.join("proj/x.nml"), &view);
        assert!(rx.try_recv().is_err(), "cached outcome pushes nothing");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Drop-NEWEST under overflow, as observed behavior (not a comment's
    /// claim): 70 real transitions through the production path against a
    /// bounded(64) channel with a live-but-unread receiver — exactly the
    /// first 64 arrive, in order. Also guards the push sites staying
    /// non-blocking (`try_send`): a refactor to `send().await`/`unwrap`
    /// fails here loudly.
    #[test]
    fn overflow_drops_newest_events() {
        let ws = temp_ws("overflow");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        let hash = publish_demo(&Store::at(&store_base));
        let pointer_path = store_base.join("schema-packages/demo/current");
        let valid = std::fs::read_to_string(&pointer_path).unwrap();
        let corrupt = "0.1.0+bad00000\nblake3:wrong\n";
        let _ = hash;

        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        std::fs::write(project.join("x.nml"), "").unwrap();

        let (tx, mut rx) = test_events();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), tx);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        // 70 flips = 70 transitions (corrupt→Failed, valid→Recovered, …),
        // receiver alive but never read: the channel fills at 64.
        for i in 0..70 {
            let content = if i % 2 == 0 { corrupt } else { valid.as_str() };
            std::fs::write(&pointer_path, content).unwrap();
            let _ = resolver.resolve(&project.join("x.nml"), &view);
        }
        let mut received = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            received.push(ev);
        }
        assert_eq!(
            received.len(),
            64,
            "bounded at capacity, no block, no panic"
        );
        for (i, ev) in received.iter().enumerate() {
            // Order preserved and the FIRST 64 kept: even = failure,
            // odd = recovery — the informative early transitions survive.
            if i % 2 == 0 {
                assert!(
                    ev.warning && ev.message.contains("failed to load"),
                    "event {i}: {ev:?}"
                );
            } else {
                assert!(
                    !ev.warning && ev.message.contains("recovered"),
                    "event {i}: {ev:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// RFC 0030 directive-vocabulary scope, case (a): a governing workspace
    /// manifest's `[]schema` names the file → its vocabulary, declared.
    #[test]
    fn vocabulary_for_declared_workspace_source() {
        use nml_validate::test_support::DEMO_MANIFEST_WITH_DIRECTIVES;
        let ws = temp_ws("vocab-declared");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES,
        )
        .unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let manifests = vec![(
            project.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES.to_string(),
        )];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        let vocab = covered(
            resolver.vocabulary_for(&project.join("core.model.nml"), &view),
            "declared source is covered",
        );
        assert!(!vocab.undeclared_sibling);
        assert_eq!(vocab.package_name, "demo");
        let names: Vec<&str> = vocab.directives.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["live", "restart", "key"]);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Case (b): not in any `[]schema`, but the unique store package binds
    /// files under this file's root → its vocabulary, undeclared. A sibling
    /// of a workspace manifest additionally sets the forgot-the-manifest
    /// flag.
    #[test]
    fn vocabulary_for_root_coverage_and_sibling_flag() {
        use nml_validate::test_support::{DEMO_CORE, DEMO_MANIFEST_WITH_DIRECTIVES};
        let ws = temp_ws("vocab-root");
        // Store variant: package present only in the store; the model file
        // sits in a subdirectory, no manifest anywhere.
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        let store = Store::at(&store_base);
        store
            .publish(&nml_validate::test_support::demo_package_with_directives())
            .expect("publish");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("schemas")).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("schemas/extra.model.nml"), DEMO_CORE).unwrap();
        let resolver = PackageResolver::new(Some(Store::at(&store_base)), test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        let vocab = covered(
            resolver.vocabulary_for(&project.join("schemas/extra.model.nml"), &view),
            "root coverage applies",
        );
        assert!(!vocab.undeclared_sibling, "store coverage is not a sibling");
        assert_eq!(vocab.package_name, "demo");

        // Workspace variant: an undeclared model file NEXT TO the manifest is
        // covered by the root rule and flagged as an undeclared sibling.
        let wsproj = ws.join("wsproj");
        std::fs::create_dir_all(&wsproj).unwrap();
        std::fs::write(
            wsproj.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES,
        )
        .unwrap();
        std::fs::write(wsproj.join("core.model.nml"), DEMO_CORE).unwrap();
        std::fs::write(wsproj.join("demo.nml"), "").unwrap();
        std::fs::write(wsproj.join("stray.model.nml"), DEMO_CORE).unwrap();
        let manifests = vec![(
            wsproj.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES.to_string(),
        )];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        let vocab = covered(
            resolver.vocabulary_for(&wsproj.join("stray.model.nml"), &view),
            "sibling is covered by the root rule",
        );
        assert!(vocab.undeclared_sibling);
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// Walk-cap honesty: when the bounded claims walk hits its entry cap
    /// before answering, the outcome is `Undetermined` naming the candidate
    /// package — never a silent Opaque — and it must NOT be memoized (the
    /// claims cache holds definitive verdicts only, so asking again re-walks
    /// and stays Undetermined instead of hardening into a cached "no").
    ///
    /// Fixture determinism: the only glob-bound file (`apps/site/app.nml`)
    /// sits in a subdirectory, and the walk finishes a directory's entries
    /// before descending — with >2048 filler entries in the root, the cap
    /// always fires before the bound file can be seen, whatever `read_dir`'s
    /// order.
    #[test]
    fn vocabulary_walk_cap_yields_undetermined_and_is_uncached() {
        use nml_validate::test_support::{DEMO_CORE, DEMO_MANIFEST_WITH_DIRECTIVES};
        let ws = temp_ws("walkcap");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site")).unwrap();
        std::fs::write(
            project.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES,
        )
        .unwrap();
        std::fs::write(project.join("core.model.nml"), DEMO_CORE).unwrap();
        std::fs::write(project.join("stray.model.nml"), DEMO_CORE).unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();
        // NOTE: no `demo.nml` root marker at top level — the bound file must
        // stay behind the filler wall. Non-.nml fillers still count against
        // the entry cap (the walk stats them before filtering).
        for i in 0..2100 {
            std::fs::write(project.join(format!("filler-{i}.txt")), "").unwrap();
        }
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let manifests = vec![(
            project.join("demo.package.nml"),
            DEMO_MANIFEST_WITH_DIRECTIVES.to_string(),
        )];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &manifests,
            doc_text: &no_docs,
        };
        for round in 0..2 {
            match resolver.vocabulary_for(&project.join("stray.model.nml"), &view) {
                VocabularyOutcome::Undetermined { candidates } => {
                    assert_eq!(candidates, ["demo"], "round {round}");
                }
                VocabularyOutcome::Covered(_) => panic!("round {round}: capped walk covered"),
                VocabularyOutcome::Opaque => panic!("round {round}: capped walk went opaque"),
            }
        }
        // The declared source is walk-free (case (a)), so the same fixture
        // stays Covered — the cap only degrades the root-coverage question.
        let vocab = covered(
            resolver.vocabulary_for(&project.join("core.model.nml"), &view),
            "declared source never depends on the walk",
        );
        assert_eq!(vocab.package_name, "demo");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// No covering package → opaque: `None`, zero vocabulary diagnostics.
    #[test]
    fn vocabulary_for_uncovered_file_is_opaque() {
        let ws = temp_ws("vocab-opaque");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("lonely.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        assert!(matches!(
            resolver.vocabulary_for(&project.join("lonely.model.nml"), &view),
            VocabularyOutcome::Opaque
        ));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn builtin_binds_package_manifests_anywhere() {
        let ws = temp_ws("builtin");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let manifest_path = project.join("demo.package.nml");
        std::fs::write(&manifest_path, MANIFEST).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.clone()];
        let view = WorkspaceView {
            roots: &roots,
            manifests: &[],
            doc_text: &no_docs,
        };
        match resolver.resolve(&manifest_path, &view).resolution {
            Resolution::Bound(b) => {
                assert_eq!(b.package_name, "nml");
                assert_eq!(b.source, DefinitionSource::Builtin);
            }
            Resolution::Unbound => panic!("manifest must bind to builtin meta package"),
        }
        let _ = std::fs::remove_dir_all(&ws);
    }
}
