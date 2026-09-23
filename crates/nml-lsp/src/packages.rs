//! Schema-package resolution for the LSP (RFC 0030) over the shared
//! workspace kernel (RFC 0019 item 0, step 0e).
//!
//! The editor no longer owns a resolver: one [`nml_validate::workspace`]
//! universe per workspace root — discovered by the kernel's bounded,
//! fail-closed walk over an [`OverlayFs`] of the unsaved buffers — answers
//! "which binding governs this file" with exactly the selection the CLI's
//! `nml check` and `nml binding` use, so the editor and the CI gate can
//! never disagree. What stays here is the editor's OWN business: where
//! external package definitions come from (the in-binary package, the
//! per-user store with its stat-guarded freshness and health events, the
//! builtin), the validator cache, the per-root universe cache and its
//! freshness guard, and the wording of the editor's degraded-state notes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nml_core::ProjectConfig;
use nml_core::diagnostic::{Severity, codes};
use nml_validate::package::{PackageError, SchemaPackage, builtin_meta_package};
use nml_validate::schema::SchemaValidator;
use nml_validate::store::{Store, StoreError};
use nml_validate::workspace::{
    ClaimClass, ClaimOrigin, Closure, Discovery, ExternalClaim, ExternalClass, Governing, Grant,
    InputKind, OverlayFs, PathFs, RootError, RootOrigin, Skip, SourceKey, Truncation, Universe,
    ValidatorMemo, WorkspaceRoot, discover, input_cap, read_input, read_leaf, resolve_file,
    walk_skips_dir,
};

pub use nml_validate::workspace::BindingStep;

/// A successful binding: everything diagnostics, hover, `nml/schemaInfo`,
/// and code actions need.
#[derive(Clone)]
pub struct Binding {
    pub package_name: String,
    pub package_version: String,
    pub content_hash: String,
    pub binding_name: String,
    pub validator: Arc<SchemaValidator>,
    /// Where the definition came from — the kernel's [`ClaimClass`] (one
    /// precedence ladder, one label vocabulary): a workspace manifest
    /// (RFC 0035 in-repo channel), an embedder's in-binary package, the
    /// per-user store's `current` slot, or the builtin meta package.
    pub class: ClaimClass,
    /// The workspace manifest the binding was read from; `None` for a
    /// package from outside the walk (injected, store, builtin).
    pub manifest: Option<PathBuf>,
    pub step: BindingStep,
    /// The directory the binding glob matched under (the claim's anchor).
    pub root: PathBuf,
    /// Set when a workspace manifest shadows a *pinned* name that the store
    /// also holds — shadowing is visible, never silent (RFC 0030).
    pub shadows_store: bool,
}

impl Binding {
    /// The human-facing binding identity — the shared `ClaimIdentity`
    /// rendering (A10), so the editor and `nml binding` print one label.
    pub fn identity(&self) -> String {
        nml_validate::workspace::ClaimIdentity {
            package: &self.package_name,
            content_hash: &self.content_hash,
            class: self.class,
        }
        .render()
    }
}

/// One diagnostic-worthy degraded state, attached at the top of the file:
/// a kernel row (a universe note, a rejection, the ambiguous claim) with
/// the severity and code the CLI prints it under — so the editor and the
/// CI gate show one verdict — or one of the editor's own advisories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedNote {
    pub message: String,
    pub severity: Severity,
    /// The stable code when the note IS a diagnostic the CLI also
    /// reports (NML2080, NML2083, NML2087–NML2091); `None` for the
    /// editor's own degraded-state advisories.
    pub code: Option<nml_core::diagnostic::Code>,
    /// Where the note sits in the resolved document.
    pub anchor: NoteAnchor,
    /// The kernel row's secondary locations (`Diagnostic::related`),
    /// each in its own file — NML2091's first failing source line —
    /// mapped to spec-native `relatedInformation` exactly as a located
    /// finding's notes are.
    pub related: Vec<nml_core::diagnostic::Related>,
    /// The kernel row's remedies (`Diagnostic::suggestions`), each
    /// stamped with the file it lands in (a failed manifest's
    /// did-you-mean names the manifest), published as the row's
    /// `data.suggestions[]` so the code-action handler offers the quick
    /// fix on that file — from the manifest document itself and from
    /// every file it would govern.
    pub suggestions: Vec<nml_core::diagnostic::Suggestion>,
    /// The code of the finding a WRAPPING row carries (`Diagnostic::cause`)
    /// when the row is located on ITS OWN document — an unloadable
    /// manifest's NML2088 at the manifest's first finding. The document's
    /// own pass reports that finding too, at the same place, under this
    /// code: the server folds the wrapper into it (RFC 0026 decision 6)
    /// rather than showing one finding twice. `None` for every other note.
    pub cause: Option<nml_core::diagnostic::Code>,
}

impl DegradedNote {
    /// A note on the document as a whole with no secondary location —
    /// the shape a refusal, a universe row and an advisory take.
    pub fn top(
        message: String,
        severity: Severity,
        code: Option<nml_core::diagnostic::Code>,
    ) -> Self {
        Self {
            message,
            severity,
            code,
            anchor: NoteAnchor::Top,
            related: Vec::new(),
            suggestions: Vec::new(),
            cause: None,
        }
    }
}

/// Where a [`DegradedNote`] sits in its document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteAnchor {
    /// The top of the file: a note about the document as a whole.
    Top,
    /// The document's first declaration — an inert input's note sits on
    /// the `package`/`project` block it declares.
    Declaration,
    /// A byte span into the document's text.
    At(nml_core::span::Span),
}

/// The kernel's vocabulary types (`nml_validate::workspace`),
/// re-exported at the resolver's seam for the server's call sites.
pub use nml_validate::workspace::{
    SchemaUniverse, UniverseState, VocabularyMatch, VocabularyOutcome,
};

/// The outcome of resolving one file.
pub enum Resolution {
    Bound(Box<Binding>),
    /// No package claims the file — today's scope-token behavior applies.
    Unbound,
    /// The universe cannot be trusted for this file — the walk was cut
    /// short (NML2089: the whole universe, or the budget unit the file
    /// sits under), or a live input FAILED TO LOAD (NML2088) and this
    /// file is content that input would govern — so NOTHING validates:
    /// the editor shows the universe's own rows and no other finding,
    /// exactly as `nml check` validates nothing and exits 1 (E28: never
    /// degrading to parse-only checking, never composing under a
    /// universe whose manifests were not all seen or loaded — NML2064
    /// used to claim `0 manifest(s) discovered` for a walk that did not
    /// finish, and `no binding governs this file` under a manifest that
    /// exists and failed). The universe's OWN input documents — a
    /// manifest, a project config, a declared `.model.nml`/`.schema.nml`
    /// source — are never refused: they keep their findings, being where
    /// the operator repairs the load. And a BOUND file whose binding
    /// cannot build its validator (NML2091: a declared source fails to
    /// load) is refused the same way — the kernel's row, pointing at the
    /// source's first finding, is the whole report; the editor used to
    /// "fall back to basic validation" there, a verdict `nml check`
    /// never gives (it refuses). The source document keeps reporting
    /// its own parse errors, being an input document. An AMBIGUOUSLY
    /// claimed file (NML2087: two live manifests claim it) is refused
    /// the same way — the CLI's one row is the whole report. And a
    /// document whose path no key can carry (past the component bound,
    /// not UTF-8) is refused with the kernel's sentence as its one
    /// error row, as `nml check` fails that target without judging it.
    Refused,
}

/// Resolution result plus any degraded-state notes to surface, and the
/// composition grant the universe gives the file.
pub struct Resolved {
    pub resolution: Resolution,
    pub notes: Vec<DegradedNote>,
    /// The file's KEY under its root, as the kernel minted it — the
    /// name every finding the editor composes carries (step 0f). `None`
    /// outside every root, or when the kernel could not mint one.
    pub key: Option<SourceKey>,
    /// What `compose_file` asks the universe about this file — the
    /// kernel's own [`Grant`], the value `nml check` composes under, so
    /// the editor and the CI gate deny or permit composition identically
    /// (NML2064/NML2065 with one sentence). Open outside every root.
    pub grant: Grant,
    /// The universe root the file resolved under and how it was fixed
    /// (`RootOrigin`, the wire's vocabulary): the workspace folder
    /// containing it (`editor`), or the root the kernel derived for a
    /// document outside every folder (`derivedVcsFence`,
    /// `derivedTargetDir`). `None` for a document with no universe.
    pub root: Option<(PathBuf, RootOrigin)>,
    /// Whether the universe DECIDES what governs this file — the
    /// kernel's own two words ([`UniverseState`]), the `--json`
    /// `binding` row's `universe`. `None` when no universe was built (a
    /// document outside every folder the kernel would not derive a root
    /// for). The editor needs it because the two UNBOUND states have
    /// different remedies and the status bar gave the OPEN one for
    /// both: over a CLOSED universe a manifest already exists and the
    /// fix is a `files` glob, not a new manifest.
    pub universe: Option<UniverseState>,
}

/// The anchor a workspace FOLDER gives a document under it: the universe
/// the kernel fixes there, or — when the folder's spelling does not
/// verify (an oracle error such as a WASI host refusing to `stat` its own
/// preopen, a folder that is no directory) — a REFUSAL the document
/// hears, in the kernel's words. It was a silent `None`: no row, no
/// note, a status bar saying "no schema" over a document judged under
/// nothing — how the bundled WASM server validated nothing on every
/// workspace and said so nowhere.
fn folder_anchor(folder: &Path, root: Result<WorkspaceRoot, RootError>) -> UniverseAnchor {
    match root {
        Ok(root) => UniverseAnchor::Folder(root),
        Err(e) => UniverseAnchor::Refused(format!(
            "cannot fix the universe at the workspace folder `{}`: {e} — the document validates \
             under nothing until the folder can be read",
            folder.display()
        )),
    }
}

/// The universe a document resolves in — R1's ladder, second and third
/// rungs (the CLI's `--root` is the first): the workspace FOLDER
/// containing it, else the root the kernel DERIVES for a document
/// outside every folder, else the kernel's refusal to derive one.
enum UniverseAnchor {
    Folder(WorkspaceRoot),
    Derived(WorkspaceRoot),
    /// The kernel refuses to derive a root (a root marker above a
    /// planted `.git` file, an unchecked shadow, the walk bound): the
    /// sentence, with the editor's advice.
    Refused(String),
}

impl UniverseAnchor {
    fn root(self) -> Option<WorkspaceRoot> {
        match self {
            Self::Folder(root) | Self::Derived(root) => Some(root),
            Self::Refused(_) => None,
        }
    }
}

/// What deriving a root for one document directory answered.
#[derive(Clone)]
enum Derivation {
    Root(WorkspaceRoot),
    /// No universe: the directory is gone or unreadable (the file is
    /// unbound, as a file outside every root always was).
    None,
    Refused(String),
}

impl Derivation {
    fn into_anchor(self) -> Option<UniverseAnchor> {
        match self {
            Self::Root(root) => Some(UniverseAnchor::Derived(root)),
            Self::None => None,
            Self::Refused(sentence) => Some(UniverseAnchor::Refused(sentence)),
        }
    }
}

/// One document directory's derivation, held until an ancestor
/// directory changes (a `.git` entry or a root marker appearing or
/// going changes its directory's mtime — the same fingerprint the
/// universe cache reads) or the buffer set changes (an unsaved marker
/// exists for the walk through the overlay).
struct DerivedRoot {
    outcome: Derivation,
    chain: Vec<(PathBuf, Fingerprint)>,
    buffers: Vec<PathBuf>,
}

/// A snapshot of the workspace the resolver needs for one pass; built by
/// the server from its own state so the resolver stays lock-free against
/// server internals.
pub struct WorkspaceView<'a> {
    /// Canonical workspace roots — each is one kernel universe.
    pub roots: &'a [PathBuf],
    /// Absolute paths of the OPEN buffers: the overlay the kernel lays over
    /// the disk (an unsaved manifest resolves live; a buffer at a path the
    /// disk lacks exists for the walk).
    pub buffers: &'a [PathBuf],
    /// The document store — every discovery read is buffer-first, and the
    /// universe's freshness guard reads its stamps.
    pub documents: &'a dyn OpenDocuments,
}

/// The editor's document store as the kernel's overlay reads it.
pub trait OpenDocuments {
    /// The stored text at `path` (an open buffer, or an indexed disk
    /// copy) — read at discovery time only.
    fn text(&self, path: &Path) -> Option<String>;
    /// The store's stamp for the document at `path`: a value that changes
    /// whenever the text is written, so a universe's freshness check
    /// compares one integer per read — never the text, which it neither
    /// clones nor scans.
    fn stamp(&self, path: &Path) -> Option<u64>;
}

/// Outcome of a store read, cached per pointer stat.
#[derive(Clone)]
enum StoreOutcome {
    Ready(Arc<SchemaPackage>),
    NotInstalled,
    /// Human-facing degraded message, already worded per the RFC contracts.
    Failed(String),
}

struct StoreCacheEntry {
    /// The pointer content at load time — the exact-by-construction
    /// freshness guard.
    pointer: Option<String>,
    outcome: StoreOutcome,
}

/// A store-package health transition or shadow warning, surfaced once via
/// `window/logMessage` (push-based, bounded, best-effort).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreEvent {
    pub message: String,
    pub warning: bool,
}

/// Fingerprint of one discovery read — or of a directory the walk
/// stopped at — for universe freshness without re-reading content: a
/// stored document's STAMP (the store's write counter at its last write —
/// one integer, so a pull that changes nothing costs no text compare and
/// no clone) or on-disk (len, mtime, mode: a flood removed changes the
/// directory's mtime, an unlistable directory made listable its mode).
#[derive(Clone, PartialEq)]
enum Fingerprint {
    Doc(u64),
    Disk(u64, Option<std::time::SystemTime>, std::fs::Permissions),
    Missing,
}

fn fingerprint_of(ws: &WorkspaceView<'_>, path: &Path) -> Fingerprint {
    if let Some(stamp) = ws.documents.stamp(path) {
        return Fingerprint::Doc(stamp);
    }
    match std::fs::metadata(path) {
        Ok(m) => Fingerprint::Disk(m.len(), m.modified().ok(), m.permissions()),
        Err(_) => Fingerprint::Missing,
    }
}

/// One root's discovered universe, held until something it read, a
/// directory that stopped its walk, the buffer set or a store pointer
/// changes (a created or deleted `.nml` file arrives as a watched event,
/// `invalidate_claims_for`) — RFC 0030 Freshness lifted from "per
/// manifest" to "per universe": the steady-state per-pull cost is stamp
/// compares and stats, never a re-walk or a re-parse.
struct CachedUniverse {
    discovery: Discovery,
    /// Everything discovery read, fingerprinted — and every directory the
    /// walk stopped at (the universe's, a unit's), so a truncation heals
    /// on the pull after its cause is gone, not at the next restart.
    reads: Vec<(PathBuf, Fingerprint)>,
    /// The buffers overlaid when the walk ran (a new or closed buffer can
    /// change a listing).
    buffers: Vec<PathBuf>,
    /// Store pointer contents when the external claims were built.
    store_pointers: Vec<(String, Option<String>)>,
}

impl CachedUniverse {
    /// The root this universe was discovered under — the discovery's own.
    fn root(&self) -> &WorkspaceRoot {
        self.discovery.root()
    }
}

/// One root's index (see [`PackageResolver::index`]).
pub struct Index {
    /// The `.nml` files to index, absolute, in the walk's order.
    pub files: Vec<PathBuf>,
    /// What the kernel denied and the editor must say — one sentence
    /// each, for `window/logMessage`.
    pub denials: Vec<String>,
}

pub struct PackageResolver {
    store: Option<Store>,
    /// An embedder-supplied package served in-process (RFC 0035 in-binary
    /// channel), hashed once.
    injected: Option<(Arc<SchemaPackage>, String)>,
    builtin: Arc<SchemaPackage>,
    store_cache: Mutex<HashMap<String, StoreCacheEntry>>,
    events: tokio::sync::mpsc::Sender<StoreEvent>,
    /// The kernel's validator table (`ValidatorMemo`), handed to every
    /// discovery so a rediscovered manifest with an unchanged hash costs
    /// no second build — the editor's own (hash, binding) cache became
    /// this table when the kernel took over building validators.
    validators: Arc<ValidatorMemo>,
    /// Resolution generation — see [`Self::generation`].
    generation: std::sync::atomic::AtomicU64,
    /// One discovered universe per root — a workspace folder's, or a
    /// derived root's (retained while an open buffer sits under it).
    universes: Mutex<HashMap<PathBuf, Arc<CachedUniverse>>>,
    /// The kernel's derivation per document DIRECTORY, for documents
    /// outside every workspace folder ([`DerivedRoot`]).
    derived: Mutex<HashMap<PathBuf, DerivedRoot>>,
    /// The wasm editor's directory listings, held while each directory's
    /// stamp is unchanged ([`crate::wasi_fs::Listings`]). Native builds
    /// list through the kernel's own `StdFs` on a filesystem whose calls
    /// cost microseconds, and hold nothing — but they carry the memo
    /// under `test`, exactly as `wasi_fs` itself is carried, so that the
    /// wiring between a watched-file event and [`Self::forget_listings`]
    /// is pinned on a lane that RUNS. Measured: with the memo compiled
    /// for wasi alone, deleting either call to `forget_listings` left
    /// every gate green, because no test lane runs on wasi.
    #[cfg(any(target_os = "wasi", test))]
    listings: crate::wasi_fs::Listings,
}

impl PackageResolver {
    pub fn new(store: Option<Store>, events: tokio::sync::mpsc::Sender<StoreEvent>) -> Self {
        Self::with_injected(store, events, None)
    }

    pub fn with_injected(
        store: Option<Store>,
        events: tokio::sync::mpsc::Sender<StoreEvent>,
        injected: Option<SchemaPackage>,
    ) -> Self {
        let builtin = Arc::new(builtin_meta_package());
        let injected = injected.map(|package| {
            let hash = package.content_hash();
            (Arc::new(package), hash)
        });
        Self {
            store,
            injected,
            builtin,
            store_cache: Mutex::new(HashMap::new()),
            events,
            validators: Arc::new(ValidatorMemo::default()),
            generation: std::sync::atomic::AtomicU64::new(0),
            universes: Mutex::new(HashMap::new()),
            derived: Mutex::new(HashMap::new()),
            #[cfg(any(target_os = "wasi", test))]
            listings: crate::wasi_fs::Listings::default(),
        }
    }

    /// Monotonic resolution generation (RFC 0010 tier 1): bumped whenever a
    /// universe is (re)discovered or a store pointer transitions.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drop EVERY cached universe — the blunt instrument.
    pub fn invalidate_claims(&self) {
        self.forget_listings();
        self.universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Drop the wasm editor's held listings. Called wherever a universe is
    /// invalidated by a DISK event: a created or deleted `.nml` file is
    /// exactly a membership change, and membership is what a listing
    /// answers — so the two caches are invalidated together or the
    /// universe is rebuilt from a listing that predates the event.
    fn forget_listings(&self) {
        #[cfg(any(target_os = "wasi", test))]
        self.listings.clear();
    }

    /// How many directories the listings memo is holding — what a test
    /// reads to see that a watched-file event reached it.
    #[cfg(test)]
    pub(crate) fn held_listings(&self) -> usize {
        self.listings.held()
    }

    /// How many document directories the DERIVED-ROOT memo is holding —
    /// what a test reads to see that closing the last buffer under a
    /// directory let its derivation go.
    #[cfg(test)]
    pub(crate) fn held_derived(&self) -> usize {
        self.derived.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether a universe is still cached under `root` — what a test
    /// reads to see that a superseded derivation took its universe with
    /// it instead of leaving one keyed on a root nothing derives now.
    #[cfg(test)]
    pub(crate) fn holds_universe_at(&self, root: &Path) -> bool {
        self.universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(root)
    }

    /// The cached universe under `root`, by identity — what a test holds
    /// to tell a universe KEPT across a re-derivation from one dropped and
    /// rebuilt (both answer `holds_universe_at`; holding the `Arc` also
    /// keeps its allocation from being reused by the rebuilt one).
    #[cfg(test)]
    fn universe_at(&self, root: &Path) -> Option<Arc<CachedUniverse>> {
        self.universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(root)
            .map(Arc::clone)
    }

    /// Drop the cached universes that watched-file creates/deletes at
    /// `paths` could have changed: exactly the roots containing a changed
    /// path; everything else is retained. An empty `paths` retains
    /// everything.
    pub fn invalidate_claims_for(&self, paths: &[PathBuf]) {
        if !paths.is_empty() {
            self.forget_listings();
        }
        self.universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|root, _| !paths.iter().any(|path| path.starts_with(root)));
    }

    /// The roots whose universes the cache holds, sorted — for the
    /// server's own pin that a removed folder's universe is dropped AT
    /// removal (the wire cannot tell: a re-added folder rediscovers on
    /// any change and serves the same content otherwise).
    #[cfg(test)]
    pub(crate) fn cached_universe_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        roots.sort();
        roots
    }

    /// The universe `path` resolves in — R1's ladder: the editor's
    /// workspace FOLDER containing it (never a derivation inside a
    /// folder), else — for a document outside every folder — the root
    /// the KERNEL derives from the document exactly as `nml check
    /// <file>` derives one (E21: the `.git` fence, the shadow refusal,
    /// the component cap), so the two front ends give one verdict there
    /// too; a project config beside such a document governs it through
    /// the kernel's nearest live config, as it governs the CLI's run.
    /// `None`: no universe at all — a document directory the oracle
    /// cannot read. A derivation the kernel REFUSES (a root marker above
    /// a planted `.git` file, an unchecked shadow, the walk bound) is
    /// [`UniverseAnchor::Refused`]: the document validates under
    /// nothing, as the CLI runs nothing there (exit 2) — and so is a
    /// workspace FOLDER the kernel cannot anchor a universe at (its
    /// spelling does not verify: absent, not a directory, an oracle
    /// error): the refusal is the document's one row and the status
    /// bar's reason, never a silent "no schema" over a document that
    /// was judged under nothing. (A file
    /// outside every folder used to be unbound under the embedder
    /// default; pre-0e the editor anchored store globs at the file's
    /// own directory — the per-file re-rooting E21 forbids, which the
    /// kernel's fenced derivation is not.)
    fn anchor_for(&self, path: &Path, ws: &WorkspaceView<'_>) -> Option<UniverseAnchor> {
        let disk = self.disk();
        let fs = OverlayFs {
            disk: &disk,
            buffers: ws.buffers,
        };
        if let Some(folder) = ws.roots.iter().find(|r| path.starts_with(r)) {
            return Some(folder_anchor(folder, WorkspaceRoot::editor(folder, &fs)));
        }
        self.derived_root_for(path, &fs, ws)
    }

    /// The kernel's derivation for a document outside every folder,
    /// memoized per document directory ([`DerivedRoot`]); derived roots
    /// and their universes live while an open buffer sits under them.
    fn derived_root_for(
        &self,
        path: &Path,
        fs: &dyn PathFs,
        ws: &WorkspaceView<'_>,
    ) -> Option<UniverseAnchor> {
        let dir = path.parent()?.to_path_buf();
        let chain: Vec<(PathBuf, Fingerprint)> = dir
            .ancestors()
            .take(nml_validate::workspace::MAX_COMPONENTS)
            .map(|d| (d.to_path_buf(), fingerprint_of(ws, d)))
            .collect();
        let buffers: Vec<PathBuf> = ws.buffers.to_vec();
        let under_a_buffer = |d: &Path| ws.buffers.iter().any(|b| b.starts_with(d));
        {
            let mut derived = self.derived.lock().unwrap_or_else(|e| e.into_inner());
            derived.retain(|d, _| under_a_buffer(d));
            if let Some(hit) = derived.get(&dir) {
                if hit.chain == chain && hit.buffers == buffers {
                    return hit.outcome.clone().into_anchor();
                }
            }
        }
        self.universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|root, u| *u.root().origin() == RootOrigin::Editor || under_a_buffer(root));
        let outcome = match WorkspaceRoot::derive(path, fs) {
            Ok(root) => Derivation::Root(root),
            Err(RootError::NotADirectory | RootError::NotAbsolute | RootError::Fs(_)) => {
                Derivation::None
            }
            // Closed-denied derivations (the CLI's usage errors): the
            // kernel's sentence, then this front end's advice — the
            // editor has no `--root`; its root is the workspace folder.
            Err(e) => Derivation::Refused(format!(
                "cannot derive a workspace root for this document: {e} — open its workspace \
                 folder, which fixes the universe"
            )),
        };
        let stale = self
            .derived
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                dir,
                DerivedRoot {
                    outcome: outcome.clone(),
                    chain,
                    buffers,
                },
            );
        // An ancestor changed (a marker or a `.git` entry appeared or
        // went) and the kernel now names ANOTHER root: the universe keyed
        // on the old one must not outlive the derivation that named it.
        // A re-derivation that names the SAME root keeps its universe —
        // an ancestor's fingerprint moves for many reasons that change
        // no verdict (a file saved beside the fence, a temp entry above
        // it), and the universe's own freshness guard re-reads everything
        // discovery read; dropping it here cost a full re-walk per such
        // save.
        if let Some(DerivedRoot {
            outcome: Derivation::Root(old),
            ..
        }) = stale
        {
            let same_root = matches!(&outcome, Derivation::Root(new) if *new == old);
            if !same_root {
                self.universes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(old.path());
            }
        }
        outcome.into_anchor()
    }

    /// The disk oracle for this build: the real filesystem natively; the
    /// wasm editor's membership-proving backend under wasi.
    #[cfg(not(target_os = "wasi"))]
    fn disk(&self) -> nml_validate::workspace::StdFs {
        nml_validate::workspace::StdFs
    }

    #[cfg(target_os = "wasi")]
    fn disk(
        &self,
    ) -> nml_validate::workspace::WasiFs<impl Fn(&Path) -> nml_validate::workspace::Listing> {
        // The shim only LISTS; the listing RULE — its sort, its kinds, and
        // the refusal of a whole listing on one unreadable entry — is the
        // kernel's one rule, the native oracle's. (A `filter_map` here
        // used to skip an entry whose kind could not be read: a manifest
        // could vanish from discovery and the universe read as OPEN.)
        // One snapshot per operation: within a single walk a directory is
        // listed at most once, so the walk cannot see a torn tree.
        let snapshot = self.listings.snapshot();
        nml_validate::workspace::wasi_fs_through(move |dir: &Path| snapshot(dir))
    }

    /// The universe of `root` — a workspace folder's or a derived one —
    /// from the cache when nothing it read has changed, else
    /// rediscovered (bumping the generation). A derived root's FIRST
    /// discovery is said once on the event channel (`window/logMessage`
    /// on the next pull), naming the root and its origin.
    fn universe_for(
        &self,
        root: &WorkspaceRoot,
        ws: &WorkspaceView<'_>,
    ) -> Option<Arc<CachedUniverse>> {
        let buffers: Vec<PathBuf> = ws
            .buffers
            .iter()
            .filter(|b| b.starts_with(root.path()))
            .cloned()
            .collect();
        let store_pointers = self.store_pointers();
        {
            let cache = self.universes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(u) = cache.get(root.path()) {
                // The anchor is part of the universe's identity: a folder
                // added over a root the kernel had derived (the same
                // path, `editor` now) rediscovers, so the origin on the
                // wire is the anchor's, not the cache's.
                let fresh = u.root() == root
                    && u.buffers == buffers
                    && u.store_pointers == store_pointers
                    && u.reads.iter().all(|(p, fp)| fingerprint_of(ws, p) == *fp);
                if fresh {
                    return Some(Arc::clone(u));
                }
            }
        }
        let built = Arc::new(self.discover_root(root, ws, buffers, store_pointers));
        self.bump_generation();
        let first = self
            .universes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(root.path().to_path_buf(), Arc::clone(&built))
            .is_none();
        if first && matches!(root.origin(), RootOrigin::Derived { .. }) {
            // The facts the CLI's root note discloses — a fence entry
            // that is no directory (a linked worktree's, a submodule's or
            // a planted `.git` file), a shadow above it — are the
            // kernel's one sentence here too, as a WARNING with this
            // front end's advice; a plain derivation is said as
            // information.
            let disclosed = root.origin().needs_disclosure();
            let facts = root
                .origin()
                .fence_facts(&|path| path.display().to_string())
                .filter(|_| disclosed)
                .map(|facts| format!(", {facts}"))
                .unwrap_or_default();
            let advice = if disclosed {
                " — open a workspace folder to fix the universe"
            } else {
                ""
            };
            let _ = self.events.try_send(StoreEvent {
                message: format!(
                    "derived a workspace root at `{}` ({}{facts}) for documents outside every \
                     workspace folder{advice}",
                    root.path().display(),
                    root.origin().tag()
                ),
                warning: disclosed,
            });
        }
        Some(built)
    }

    fn store_pointers(&self) -> Vec<(String, Option<String>)> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let mut names = store.list_names();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                let p = store.pointer_content(&n);
                (n, p)
            })
            .collect()
    }

    fn discover_root(
        &self,
        root: &WorkspaceRoot,
        ws: &WorkspaceView<'_>,
        buffers: Vec<PathBuf>,
        store_pointers: Vec<(String, Option<String>)>,
    ) -> CachedUniverse {
        let disk = self.disk();
        let fs = OverlayFs {
            disk: &disk,
            buffers: &buffers,
        };
        let reads: RefCell<Vec<(PathBuf, Fingerprint)>> = RefCell::new(Vec::new());
        let read = |kind: InputKind, path: &Path| -> Result<String, String> {
            reads
                .borrow_mut()
                .push((path.to_path_buf(), fingerprint_of(ws, path)));
            match ws.documents.text(path) {
                // A buffer-served input is capped exactly as a disk read
                // (E39: one verdict, one sentence): the index holds files
                // up to 16 MiB — four times a declared source's cap — and
                // served the whole text to discovery, so a 5 MiB source
                // the CLI refuses (NML2088, nothing validated) loaded
                // here and the tenant's file was judged under it.
                Some(text) if text.len() > input_cap(kind) => {
                    Err(nml_validate::workspace::too_large(
                        Some(text.len() as u64),
                        input_cap(kind),
                        &format!("a {}", kind.label()),
                    ))
                }
                Some(text) => Ok(text),
                // The disk case is the kernel's own (`read_input`: the
                // race-free chain under the root, the kind's cap, one
                // sentence) — the same call the CLI makes.
                None => read_input(root, kind, path),
            }
        };
        let mut extra: Vec<ExternalClaim> = Vec::new();
        if let Some((package, _)) = &self.injected {
            extra.push(ExternalClaim::new(
                Arc::clone(package),
                ExternalClass::Injected,
            ));
        }
        for (name, _) in &store_pointers {
            if let StoreOutcome::Ready(package) = self.load_store_package(name) {
                extra.push(ExternalClaim::new(package, ExternalClass::Store));
            }
        }
        extra.push(ExternalClaim::new(
            Arc::clone(&self.builtin),
            ExternalClass::Builtin,
        ));
        let discovery = discover(root, &fs, &read, extra, Arc::clone(&self.validators));
        let mut reads = reads.into_inner();
        let stops = discovery
            .truncated()
            .map(|t| match t {
                Truncation::Entries { dir } | Truncation::Unreadable { dir, .. } => dir,
                Truncation::LiveInputBytes { key } | Truncation::TotalLiveInputBytes { key } => key,
            })
            .into_iter()
            .chain(discovery.truncated_units().iter().map(|unit| &unit.stop));
        for stop in stops {
            let path = root.path_of(stop);
            let fingerprint = fingerprint_of(ws, &path);
            reads.push((path, fingerprint));
        }
        CachedUniverse {
            discovery,
            reads,
            buffers,
            store_pointers,
        }
    }

    /// The editor's index of one workspace root (step 0e-b): the `.nml`
    /// files the kernel's ONE walk enumerated under it
    /// (`Discovery::nml_files_under`), so nothing indexed can be
    /// unresolvable and nothing resolvable is unindexed — there is no
    /// second walk under a second bound. What the kernel denies it does
    /// not index, and says so in [`Index::denials`], once per root, in the
    /// CLI's own words: a truncated universe (NML2089) indexes NOTHING —
    /// fail-closed, as `nml check` validates nothing under it, where the
    /// old index walk silently kept the first ten thousand files — a
    /// spent budget unit's files are absent, and a live input that failed
    /// to load (NML2088) is named (the walk completed, so its files are
    /// indexed, every one of them unbound). `root` must be canonical.
    pub fn index(&self, root: &Path, ws: &WorkspaceView<'_>) -> Index {
        let disk = self.disk();
        let fs = OverlayFs {
            disk: &disk,
            buffers: ws.buffers,
        };
        // A folder the oracle cannot canonicalize (removed while the
        // editor was open, no buffer under it): no universe — nothing
        // binds, nothing closes, nothing is indexed.
        let Some(u) = WorkspaceRoot::editor(root, &fs)
            .ok()
            .and_then(|root| self.universe_for(&root, ws))
        else {
            return Index {
                files: Vec::new(),
                denials: Vec::new(),
            };
        };
        let files = u
            .discovery
            .nml_files_under(&SourceKey::root())
            .map(|key| u.root().path_of(key))
            .collect();
        let coded = |d: &nml_core::diagnostic::Diagnostic| match d.code {
            Some(code) => format!("[{code}] {}", d.message),
            None => d.message.clone(),
        };
        let mut denials: Vec<String> = Vec::new();
        for error in u.discovery.universe_errors() {
            if error.code == Some(nml_core::diagnostic::codes::UNIVERSE_TRUNCATED) {
                denials.push(format!(
                    "nothing under `{}` is indexed: {}",
                    u.root().path().display(),
                    coded(&error)
                ));
            } else {
                denials.push(coded(&error));
            }
        }
        // A denied unit's row carries its unit — the `source` the
        // kernel stamps on it, read back the way every key that left
        // the kernel comes back (`SourceKey::checked`) — never a
        // parallel list zipped by position.
        for error in u.discovery.unit_errors() {
            let unit = error.source.as_deref().and_then(SourceKey::checked);
            denials.push(match unit {
                Some(unit) => format!(
                    "nothing under `{}` is indexed: {}",
                    u.root().path_of(&unit).display(),
                    coded(&error)
                ),
                None => coded(&error),
            });
        }
        // A directory the walk could NOT enter — at the component bound,
        // or named so that no key can carry it — holds content the index
        // never saw: said in the kernel's own row (NML2090, the CLI's
        // gate sentence), the fail-closed reasons only. What the walk
        // skips BY POLICY (a dot-directory, a link, a FIFO,
        // `node_modules`, `target`) the editor skips by the same policy
        // and says nothing, as it never did.
        let closed = u.discovery.universe().is_closed();
        for skipped in u.discovery.skipped() {
            let fail_closed = match skipped.why {
                Skip::ComponentBound => true,
                // The gate fails on EVERY entry so named — a directory it
                // never entered, a link it never read, a `.nml` file or
                // special entry no verb judged — and so does the index.
                Skip::UnkeyableName { .. } => true,
                Skip::Symlink
                | Skip::Fifo
                | Skip::DotDirectory
                | Skip::DotFile
                | Skip::PolicyDirectory => false,
            };
            if !fail_closed {
                continue;
            }
            if let Some(row) = nml_validate::workspace::skipped(skipped, closed) {
                denials.push(coded(&row));
            }
        }
        Index { files, denials }
    }

    /// Resolve one file. `path` must be absolute and canonical.
    pub fn resolve(&self, path: &Path, ws: &WorkspaceView<'_>) -> Resolved {
        let mut notes = Vec::new();
        // No universe at all: the file is unbound, open context.
        let unrooted = |notes: Vec<DegradedNote>| Resolved {
            resolution: Resolution::Unbound,
            notes,
            key: None,
            grant: Grant::open(),
            root: None,
            universe: None,
        };
        let root = match self.anchor_for(path, ws) {
            None => return unrooted(notes),
            Some(UniverseAnchor::Folder(root) | UniverseAnchor::Derived(root)) => root,
            // The kernel refuses to derive a root for a document outside
            // every folder: the row is the whole report and the document
            // validates under nothing (closed-denied), as the CLI runs
            // nothing there.
            Some(UniverseAnchor::Refused(sentence)) => {
                notes.push(DegradedNote::top(sentence, Severity::Error, None));
                return Resolved {
                    resolution: Resolution::Refused,
                    notes,
                    key: None,
                    grant: Grant::open(),
                    root: None,
                    // No root was fixed, so no universe was built.
                    universe: None,
                };
            }
        };
        // A document outside every folder is spelled through ITS root by
        // the one path rule (canonical above the derived root, untouched
        // below — an author's link stays a link for the kernel to judge).
        let path = canonical_above_roots(path.to_path_buf(), &[root.path().to_path_buf()]);
        let path = path.as_path();
        let Some(u) = self.universe_for(&root, ws) else {
            return unrooted(notes);
        };
        let root_facts = Some((u.root().path().to_path_buf(), u.root().origin().clone()));
        let universe = u.discovery.universe();
        let disk = self.disk();
        let fs = OverlayFs {
            disk: &disk,
            buffers: &u.buffers,
        };
        let resolved = match resolve_file(&universe, path, &fs) {
            Ok(r) => r,
            // The kernel cannot key the document (past the component
            // bound, a component that is not UTF-8): `nml check` fails
            // that target with this sentence and judges nothing, so the
            // editor refuses it with the same sentence as its one error
            // row — never a warning over a document validated in the
            // open registry mode, a verdict the CLI never gives.
            Err(e) => {
                notes.push(DegradedNote::top(e.to_string(), Severity::Error, None));
                // No key: the universe's word on an unnamed file is its
                // closure alone.
                let grant = Grant::unbound(&universe);
                return Resolved {
                    resolution: Resolution::Refused,
                    notes,
                    key: None,
                    grant,
                    root: root_facts,
                    universe: Some(universe.state()),
                };
            }
        };
        let nml_validate::workspace::Resolved {
            key,
            governing,
            findings,
            grant,
            validator,
            ..
        } = resolved;
        // The universe's word — the rows `nml check` states once per run,
        // by the kernel's ONE rule for both front ends
        // (`Discovery::universe_notes`: its errors, else its unit-layout
        // notes; a universe that cannot be trusted lints nothing): an
        // error rides every document under the universe; a layout note
        // (NML2092) rides the MANIFEST document that carries the glob
        // and no other. A row whose `source` is this document and that
        // carries a span — the lint at its glob, NML2081 at its item —
        // sits AT the span, as the CLI's row names its line and column.
        for d in u.discovery.universe_notes() {
            let own = d.source.as_deref() == Some(key.as_str());
            if d.code == Some(codes::BUDGET_UNIT_GAP) && !own {
                continue;
            }
            let anchor = match d.span {
                Some(span) if own => NoteAnchor::At(span),
                _ => NoteAnchor::Top,
            };
            // On its own document a wrapping row's finding is the
            // document's own too (the parse band, the meta-validation):
            // the code lets the server fold the two into one row.
            let cause = match anchor {
                NoteAnchor::At(_) => d.cause.as_ref().map(|c| c.code),
                _ => None,
            };
            // A row located in ANOTHER document — an unloadable manifest's
            // first finding, shown on a file under it — keeps its place
            // as a related location there: the sentence names no line
            // (the location is the row's own, stated once).
            // The row's sentence as the CLI prints it — the did-you-mean
            // hint the row's remedy derives included (`rendered_message`,
            // the one renderer every surface shares).
            let message = d.rendered_message();
            let d = match (own, d.span, d.source.clone()) {
                (false, Some(span), Some(source)) => {
                    d.with_related_in(span, "the manifest's finding", Some(source))
                }
                _ => d,
            };
            notes.push(DegradedNote {
                message,
                severity: d.severity,
                code: d.code,
                anchor,
                related: d.related,
                suggestions: d.suggestions,
                cause,
            });
        }
        // The kernel's own findings on this key (NML2083, the ambiguous
        // claim, a denied unit), each under its own severity and code.
        for d in &findings {
            notes.push(DegradedNote {
                message: d.message.clone(),
                severity: d.severity.clone(),
                code: d.code,
                anchor: NoteAnchor::Top,
                related: d.related.clone(),
                suggestions: d.suggestions.clone(),
                cause: None,
            });
        }
        // An inert input (NML2080) is reported ONCE, on ITS OWN document,
        // at its declaration, as information. The CLI prints the note
        // once per run above the files it bears on; the editor showed it
        // on EVERY file beneath the input, forever, at 1:1, as a warning
        // its reader could not act on (three permanent rows on every
        // tenant file). The input's author is the
        // one who can act, and their document is where they look.
        for d in u
            .discovery
            .inert()
            .iter()
            .filter(|d| d.source.as_deref() == Some(key.as_str()))
        {
            notes.push(DegradedNote {
                message: d.message.clone(),
                severity: Severity::Info,
                code: d.code,
                anchor: NoteAnchor::Declaration,
                related: Vec::new(),
                suggestions: Vec::new(),
                cause: None,
            });
        }
        // Manifest-side shadow warnings belong to THE MANIFEST DOCUMENT
        // only (spans are byte offsets into its text).
        if let Some(claim) = u
            .discovery
            .claims()
            .iter()
            .find(|c| c.manifest().is_some_and(|m| *m == key))
        {
            for w in claim.package.manifest.shadow_warnings() {
                notes.push(DegradedNote {
                    message: w.message,
                    severity: Severity::Warning,
                    code: None,
                    anchor: w.span.map_or(NoteAnchor::Top, NoteAnchor::At),
                    related: Vec::new(),
                    suggestions: Vec::new(),
                    cause: None,
                });
            }
        }
        // Pins that name no live definition: the editor's own advisories
        // (the kernel skips such a pin silently, by R5).
        self.pin_notes(&universe, &key, &mut notes);

        // A walk that did not finish — for the universe, or for the
        // unit the file sits under — validates nothing (the notes above
        // carry the NML2089 row that says so). Nor does a universe whose
        // live input FAILED TO LOAD (NML2088) validate the content it
        // would govern: `nml check` exits 1 there before any target, and
        // a verdict from the registry — NML2064 "no binding governs this
        // file", a model finding from a package the manifest never bound
        // — is one the CLI never gives. The universe's own input
        // documents keep their findings (the note above still names the
        // load error): they are where the operator repairs it.
        let input_document = {
            let name = key.file_name();
            nml_validate::workspace::is_manifest_name(name)
                || name == nml_validate::workspace::PROJECT_CONFIG_NAME
                || nml_validate::workspace::is_schema_source_name(name)
        };
        let unloadable = matches!(universe.closure, Closure::Unloadable(_));
        // An ERROR among the kernel's own findings ON THIS KEY is the
        // CLI SKIPPING the target: a closed universe rejecting a
        // symlinked path (NML2083), a name no key can carry. `nml check`
        // states the row, counts the file under `skipped.byWhy`, exits 1
        // — and judges NOTHING. The editor published the same row and
        // then resolved `Unbound`, so the document went on to be
        // validated in the open registry mode: a second verdict, on a
        // file the CLI refused to read, which the parity harness
        // (`tests/integration/parity.rs`) is what found.
        let key_refused = findings
            .iter()
            .any(|d| matches!(d.severity, Severity::Error));
        if u.discovery.truncated().is_some()
            || universe.truncated_unit(&key).is_some()
            || key_refused
            || (unloadable && !input_document)
        {
            return Resolved {
                resolution: Resolution::Refused,
                notes,
                key: Some(key),
                grant,
                root: root_facts.clone(),
                universe: Some(universe.state()),
            };
        }

        let resolution = match &governing {
            Governing::Bound { claimant, step } => {
                let claim = claimant.claim;
                // The kernel built the binding's validator once for the
                // universe, or minted NML2091 (in `notes` above): then
                // the file validates under NOTHING, as `nml check`
                // validates nothing — never "basic validation" under a
                // binding the CLI refuses.
                let Some(validator) = validator else {
                    return Resolved {
                        resolution: Resolution::Refused,
                        notes,
                        key: Some(key),
                        grant,
                        root: root_facts.clone(),
                        universe: Some(universe.state()),
                    };
                };
                // The claim's class IS the source (the kernel's one
                // ladder); a workspace claim carries its manifest key by
                // construction (`ClaimOrigin`), so there is no arm for a
                // claim without one.
                let manifest = match claim.origin() {
                    ClaimOrigin::Workspace { manifest, .. } => Some(u.root().path_of(manifest)),
                    ClaimOrigin::External { .. } => None,
                };
                let shadows_store = matches!(claim.origin(), ClaimOrigin::Workspace { .. })
                    && *step == BindingStep::Pinned
                    && self.store_has(claim.name());
                if shadows_store {
                    notes.push(DegradedNote::top(
                        format!(
                            "bound by workspace manifest for '{}', shadowing the store's copy",
                            claim.name()
                        ),
                        Severity::Info,
                        None,
                    ));
                }
                Resolution::Bound(Box::new(Binding {
                    package_name: claim.name().to_string(),
                    package_version: claim.package.manifest.version.clone(),
                    content_hash: claim.content_hash().to_string(),
                    binding_name: claimant.binding.name.clone(),
                    validator,
                    class: claim.class(),
                    manifest,
                    step: *step,
                    root: u.root().path_of(&claimant.anchor),
                    shadows_store,
                }))
            }
            // An ambiguously claimed file is DENIED before it is read, as
            // `nml check` denies it: the kernel's NML2087 row (in `notes`
            // above, naming every claimant) is its whole report — nothing
            // parses, nothing composes. The ambiguous form of NML2064 is
            // an embedder's verdict for a file it chose to compose anyway;
            // the editor used to compose it and publish both rows, a
            // second verdict the CLI never gives.
            Governing::Ambiguous(_) => Resolution::Refused,
            Governing::Unbound => Resolution::Unbound,
        };
        Resolved {
            resolution,
            notes,
            key: Some(key),
            grant,
            root: root_facts.clone(),
            universe: Some(universe.state()),
        }
    }

    /// The nearest LIVE project config governing `path` (R5: pins and
    /// tooling fields come from the nearest live config; an inert
    /// tenant-committed one is content) — its path (through the overlay:
    /// an unsaved buffer at that path IS the config) and its parsed
    /// content. `None` outside every root or with no live config on the
    /// chain.
    fn nearest_live_config(
        &self,
        path: &Path,
        ws: &WorkspaceView<'_>,
    ) -> Option<(PathBuf, ProjectConfig)> {
        let root = self.anchor_for(path, ws)?.root()?;
        let path = canonical_above_roots(path.to_path_buf(), &[root.path().to_path_buf()]);
        let u = self.universe_for(&root, ws)?;
        let key = SourceKey::under(u.root(), &path, &self.disk())?;
        u.discovery
            .universe()
            .nearest_config(&key)
            .map(|c| (u.root().path_of(&c.path), c.config.clone()))
    }

    /// The nearest live project config's CONTENT — what a document's
    /// tooling fields resolve under (the nearest live config's).
    pub fn project_config_for(&self, path: &Path, ws: &WorkspaceView<'_>) -> Option<ProjectConfig> {
        self.nearest_live_config(path, ws).map(|(_, config)| config)
    }

    /// The nearest live project config's PATH — the file a pin or
    /// opt-out for `path` belongs in, by the one rule the kernel
    /// resolves pins under (the nearest live config's): never an
    /// inert tenant-committed config (a pin there changes nothing), and
    /// an unsaved config the overlay resolves through is the file.
    pub fn project_config_path_for(&self, path: &Path, ws: &WorkspaceView<'_>) -> Option<PathBuf> {
        self.nearest_live_config(path, ws).map(|(path, _)| path)
    }

    fn pin_notes(&self, universe: &Universe<'_>, key: &SourceKey, notes: &mut Vec<DegradedNote>) {
        let Some(config) = universe.nearest_config(key) else {
            return;
        };
        for pin in config.config.pinned_packages() {
            if !nml_validate::package::valid_package_name(&pin) {
                notes.push(DegradedNote::top(
                    format!(
                        "schema package name {pin:?} is not a valid package name ([a-z][a-z0-9-]*) — ignored (from schemaPackages or provider.tool)"
                    ),
                    Severity::Warning,
                    None,
                ));
                continue;
            }
            if universe.claims.iter().any(|c| c.name() == pin) {
                continue;
            }
            // The pin names a package explicitly — its store failure is
            // this file's business; an absent one gets the sync hint.
            match self.load_store_package(&pin) {
                StoreOutcome::Failed(message) => {
                    notes.push(DegradedNote::top(message, Severity::Warning, None))
                }
                StoreOutcome::NotInstalled if self.store.is_some() => {
                    notes.push(DegradedNote::top(
                        format!(
                            "pinned schema package '{pin}' is not installed — run '{pin} schema sync'"
                        ),
                        Severity::Warning,
                        None,
                    ))
                }
                _ => {}
            }
        }
    }

    /// The directive vocabulary covering `path` (RFC 0030) — the kernel's
    /// answer ([`Discovery::vocabulary_for`]) for the universe this editor
    /// anchors the path in: a declared `[]schema` source, else the unique
    /// coverer of its directory, else opaque; a truncated universe answers
    /// `Undetermined`.
    pub fn vocabulary_for(&self, path: &Path, ws: &WorkspaceView<'_>) -> VocabularyOutcome {
        let Some((root, u)) = self
            .anchor_for(path, ws)
            .and_then(UniverseAnchor::root)
            .and_then(|root| self.universe_for(&root, ws).map(|u| (root, u)))
        else {
            return VocabularyOutcome::Opaque;
        };
        let path = canonical_above_roots(path.to_path_buf(), &[root.path().to_path_buf()]);
        if u.discovery.truncated().is_some() {
            return VocabularyOutcome::Undetermined;
        }
        let Some(key) = SourceKey::under(u.root(), &path, &self.disk()) else {
            return VocabularyOutcome::Opaque;
        };
        u.discovery.vocabulary_for(&key)
    }

    fn store_has(&self, name: &str) -> bool {
        self.store
            .as_ref()
            .is_some_and(|s| s.pointer_content(name).is_some())
    }

    /// Store read with a stat-guarded cache: the per-pass probe is a
    /// `stat`; the package is re-read and re-hashed only on a pointer
    /// transition (RFC 0030 Freshness). Health transitions surface once,
    /// at the transition, on the event channel.
    fn load_store_package(&self, name: &str) -> StoreOutcome {
        let Some(store) = self.store.as_ref() else {
            return StoreOutcome::NotInstalled;
        };
        let pointer = store.pointer_content(name);
        let mut cache = self.store_cache.lock().unwrap_or_else(|e| e.into_inner());
        match cache.get(name) {
            Some(e) if e.pointer == pointer => e.outcome.clone(),
            prior => {
                let outcome = match &pointer {
                    None => StoreOutcome::NotInstalled,
                    Some(content) => match store.load_current(name, content) {
                        Ok(slot) => {
                            for w in slot.package.manifest.shadow_warnings() {
                                let _ = self.events.try_send(StoreEvent {
                                    message: format!("schema package '{name}': {}", w.message),
                                    warning: true,
                                });
                            }
                            StoreOutcome::Ready(Arc::new(slot.package))
                        }
                        Err(StoreError::NotInstalled) => StoreOutcome::NotInstalled,
                        Err(StoreError::Package(PackageError::UnsupportedFormatVersion {
                            required,
                            supported,
                        })) => StoreOutcome::Failed(format!(
                            "schema package '{name}' needs formatVersion {required}; this nml-lsp supports {supported} — update nml-lsp; the package binds nothing until then"
                        )),
                        Err(e) => StoreOutcome::Failed(format!(
                            "schema package '{name}' in the store failed to load: {e} — the package binds nothing until then"
                        )),
                    },
                };
                let was_failed = matches!(prior.map(|e| &e.outcome), Some(StoreOutcome::Failed(_)));
                match (&outcome, was_failed) {
                    (StoreOutcome::Failed(message), false) => {
                        let _ = self.events.try_send(StoreEvent {
                            message: message.clone(),
                            warning: true,
                        });
                    }
                    (StoreOutcome::Ready(_), true) => {
                        let _ = self.events.try_send(StoreEvent {
                            message: format!("schema package '{name}' in the store recovered"),
                            warning: false,
                        });
                    }
                    _ => {}
                }
                self.bump_generation();
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
}

/// The disk stamp of an indexed copy's file — `(len, mtime)`, the two of
/// [`Fingerprint::Disk`]'s three that a CONTENT change moves (the mode is
/// the walk's concern, not a text's). `None` when the file cannot be
/// stat-ed at all, which is the watcher's case (a deletion), not this
/// one's.
pub(crate) fn disk_stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// The input kind a path is read as, by its name — the KERNEL's rule
/// ([`InputKind::of_name`]), so an action reads a file under the cap the
/// walk reads that kind with. The editor keeps no name rule of its own:
/// its copy admitted a bare `package.nml` as a manifest, which the
/// kernel's `is_manifest_name` deliberately refuses.
pub(crate) fn input_kind_of(path: &Path) -> InputKind {
    InputKind::of_name(path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
}

/// A discovery-KIND input read OUTSIDE the walk — a quick fix's target,
/// a declared source a code action re-reads — under the kernel's
/// per-kind cap ([`input_cap`]) through the kernel's one reader at the
/// file's own leaf ([`read_leaf`]: the open never blocks, a non-regular
/// file or a leaf swapped for a link is refused); the refusal is the
/// kernel's own sentence, the one the CLI prints for the same file
/// (`too large: 5 MiB (5242880 bytes) — a declared schema source is read
/// only up to 4 MiB (4194304 bytes)`). The walk's own inputs go through
/// [`read_input`] under their root; these have no root in hand. The
/// editor keeps no reader of its own: every disk read it makes is the
/// kernel's, and a source ratchet holds it there.
pub(crate) fn read_input_at_leaf(kind: InputKind, path: &Path) -> Result<String, String> {
    read_leaf(path, input_cap(kind), &format!("a {}", kind.label())).map_err(|e| e.to_string())
}

/// Could a watched CREATE/DELETE at `path` change any cached universe?
/// Only `.nml` paths a discovery walk could SEE matter: anything under a
/// policy-skipped segment ([`walk_skips_dir`]) is invisible to every walk.
/// Segments are judged ROOT-relative, exactly like the walk. Outside every
/// workspace root there is nothing to keep fresh.
pub(crate) fn watched_path_affects_claims(path: &Path, roots: &[PathBuf]) -> bool {
    if !path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(nml_validate::workspace::is_nml_name)
    {
        return false;
    }
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|rel| {
            rel.parent().is_some_and(|dirs| {
                !dirs.components().any(|component| match component {
                    std::path::Component::Normal(name) => name.to_str().is_some_and(walk_skips_dir),
                    _ => false,
                })
            })
        })
    })
}

/// A document path with the prefix ABOVE its root canonical and nothing
/// below it resolved: macOS's `/tmp` → `/private/tmp` and an operator's
/// symlinked checkout resolve (every workspace folder was canonicalized
/// at initialize, a derived root by the kernel's fence-aware walk, so
/// the prefix must match one), while a link an author committed INSIDE
/// the root stays a link for the kernel to judge — the CLI refuses it
/// (NML2083); the editor used to resolve it silently to its target and
/// judge THAT file under whatever binding claims it. Outside every root
/// the path stays AS SPELLED: it is what the kernel derives a root from
/// (`WorkspaceRoot::derive` follows links only above the fence — a
/// pre-canonicalized path would derive from an author-planted link's
/// TARGET, E28(2)), and the document store keys by the client's own
/// spelling. The outermost matching root wins, and so does the resolver's
/// pick: `NmlLanguageServer::workspace_roots` is kept SORTED, so the first
/// root a path starts with IS the outermost one (before that the client's
/// `workspaceFolders` order decided between two nested folders, and this
/// rule and the resolver's could disagree).
pub(crate) fn canonical_above_roots(path: PathBuf, roots: &[PathBuf]) -> PathBuf {
    let ancestors: Vec<&Path> = path.ancestors().collect();
    for ancestor in ancestors.iter().rev() {
        let Ok(canonical) = dunce::canonicalize(ancestor) else {
            continue;
        };
        if roots.contains(&canonical) {
            if let Ok(rest) = path.strip_prefix(ancestor) {
                return canonical.join(rest);
            }
        }
    }
    path
}

/// A file's NAME on every finding the editor composes and every dedup
/// key (step 0f): its workspace key under a root — root-relative,
/// `/`-separated, the spelling `nml check --json` prints as `source` —
/// and its absolute path outside every root, where there is no universe
/// to key it in.
pub(crate) fn source_name_of(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .filter(|rel| !rel.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// A shadow's path for the wire (`nml/schemaInfo`'s `rootShadowed`),
/// spelled FROM the derived root it sits above — `../../.git`,
/// `../demo.package.nml`: never absolute (the payload's rule, as
/// [`display_path`]), and saying how far above the root it sits, which
/// the bare file name would not. A path that is not above the root (the
/// kernel derives no such shadow) falls back to its display form.
pub(crate) fn shadow_display(root: &Path, shadow: &Path) -> String {
    let name = shadow
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match shadow.parent().and_then(|dir| root.strip_prefix(dir).ok()) {
        Some(below) => format!("{}{name}", "../".repeat(below.components().count())),
        None => display_path(shadow, &[]),
    }
}

/// A path for user-facing messages: workspace-root-relative, `/`-separated,
/// falling back to the file name outside every root. Never absolute.
pub(crate) fn display_path(path: &Path, roots: &[PathBuf]) -> String {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .filter(|rel| !rel.is_empty())
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    /// A workspace folder whose spelling the oracle cannot verify — here
    /// the WASI backend's own `NoRealpath`, the shape a host refusing to
    /// `stat` its preopen takes — is a REFUSAL the document hears (its one
    /// row, the status bar's reason), in the kernel's words, never a
    /// silent `None`; a folder that verifies anchors the universe.
    #[test]
    fn a_folder_the_oracle_refuses_is_a_loud_refusal_never_silence() {
        use nml_validate::workspace::{FsError, RootError, StdFs, WorkspaceRoot};
        let folder = std::path::Path::new("/workspace");
        match super::folder_anchor(folder, Err(RootError::Fs(FsError::NoRealpath))) {
            super::UniverseAnchor::Refused(sentence) => {
                assert!(
                    sentence.starts_with(
                        "cannot fix the universe at the workspace folder `/workspace`: "
                    ),
                    "{sentence}"
                );
                assert!(sentence.contains("validates under nothing"), "{sentence}");
            }
            _ => panic!("a refused folder must be a loud refusal, never a silent anchor"),
        }
        let real = std::env::temp_dir();
        let root = WorkspaceRoot::editor(&real, &StdFs).expect("a real directory anchors");
        assert!(matches!(
            super::folder_anchor(&real, Ok(root)),
            super::UniverseAnchor::Folder(_)
        ));
    }

    use super::*;

    use nml_validate::test_support::{DEMO_CORE as CORE, DEMO_MANIFEST as MANIFEST, publish_demo};
    use nml_validate::workspace::ReadError;

    /// A guard-owned scratch workspace (removed on drop, a red assertion
    /// included), canonicalized like every root the server holds.
    fn temp_ws(tag: &str) -> crate::scratch::Scratch {
        crate::scratch::Scratch::new(&format!("pkg-test-{tag}"))
    }

    /// An empty document store.
    struct NoDocs;

    impl OpenDocuments for NoDocs {
        fn text(&self, _: &Path) -> Option<String> {
            None
        }

        fn stamp(&self, _: &Path) -> Option<u64> {
            None
        }
    }

    /// A store holding ONE document, its stamp and text settable, counting
    /// every text read the resolver makes.
    struct OneDoc {
        path: PathBuf,
        stamp: std::cell::Cell<u64>,
        text: RefCell<String>,
        texts_read: std::cell::Cell<usize>,
    }

    impl OneDoc {
        fn new(path: PathBuf, text: &str) -> Self {
            Self {
                path,
                stamp: std::cell::Cell::new(1),
                text: RefCell::new(text.to_string()),
                texts_read: std::cell::Cell::new(0),
            }
        }
    }

    impl OpenDocuments for OneDoc {
        fn text(&self, path: &Path) -> Option<String> {
            (path == self.path).then(|| {
                self.texts_read.set(self.texts_read.get() + 1);
                self.text.borrow().clone()
            })
        }

        fn stamp(&self, path: &Path) -> Option<u64> {
            (path == self.path).then(|| self.stamp.get())
        }
    }

    /// A store OUTSIDE the workspace root. (Inside it, the slot's own
    /// `demo.package.nml` is a discovered WORKSPACE manifest — a same-named
    /// workspace definition shadows the store's copy universe-wide, rule
    /// 3 — which is exactly what the pre-0e editor could not see and
    /// what `Store::user()` never does.)
    fn store_dir(tag: &str) -> crate::scratch::Scratch {
        temp_ws(&format!("{tag}-store"))
    }

    fn view<'a>(roots: &'a [PathBuf]) -> WorkspaceView<'a> {
        WorkspaceView {
            roots,
            buffers: &[],
            documents: &NoDocs,
        }
    }

    /// The editor reads a file outside the walk under the KERNEL's cap
    /// for that name ([`InputKind::of_name`]) — it keeps no suffix rule
    /// of its own. Its own copy classified a bare `package.nml` as a
    /// manifest and read it under the 256 KiB manifest cap, where the
    /// kernel's `is_manifest_name` refuses that name as a manifest and
    /// the walk would read it as a source (4 MiB).
    #[test]
    fn input_kind_is_the_kernels_name_rule() {
        for (name, want) in [
            ("demo.package.nml", InputKind::Manifest),
            ("package.nml", InputKind::Source),
            ("nml-project.nml", InputKind::ProjectConfig),
            ("core.model.nml", InputKind::Source),
        ] {
            assert_eq!(
                input_kind_of(Path::new("/ws").join(name).as_path()),
                want,
                "{name}"
            );
            assert_eq!(
                input_kind_of(Path::new("/ws").join(name).as_path()),
                InputKind::of_name(name),
                "{name}"
            );
        }
    }

    /// r84-cov (mutant L3 survived): the leaf read's cap is INCLUSIVE —
    /// exactly `cap` bytes read whole, one more is refused in the
    /// kernel's one cap sentence — the index cap and the discovery input
    /// caps alike. The rule is the kernel reader's (pinned there too);
    /// this pins that the editor's leaf read IS that reader.
    #[test]
    fn the_leaf_read_is_inclusive_at_the_cap() {
        let ws = temp_ws("read-bounded-cap");
        let exact = ws.join("exact.nml");
        std::fs::write(&exact, vec![b' '; 64]).unwrap();
        let over = ws.join("over.nml");
        std::fs::write(&over, vec![b' '; 65]).unwrap();
        assert_eq!(
            read_leaf(&exact, 64, "a test input").unwrap().len(),
            64,
            "exactly the cap reads whole"
        );
        assert!(
            matches!(
                read_leaf(&over, 64, "a test input"),
                Err(ReadError::Refused(sentence))
                    if sentence
                        == "too large: 65 bytes (65 bytes) — a test input is read only up to 64 \
                            bytes (64 bytes)"
            ),
            "one byte more is refused in the kernel's sentence"
        );
    }

    /// r84-cov (mutant L5 survived): a spent budget UNIT is said too — its
    /// files are absent from the index and ONE denial names the unit,
    /// while the rest of the root is indexed (the CLI's NML2089 unit row,
    /// in the editor's `window/logMessage` words).
    #[cfg(unix)]
    #[test]
    fn the_index_says_a_spent_unit_and_indexes_the_rest() {
        use std::os::unix::fs::PermissionsExt;
        let ws = temp_ws("index-unit-denied");
        std::fs::write(ws.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        std::fs::create_dir_all(ws.join("apps/cu/locked")).unwrap();
        std::fs::create_dir_all(ws.join("apps/du")).unwrap();
        std::fs::write(ws.join("apps/cu/app.nml"), "").unwrap();
        std::fs::write(ws.join("apps/du/app.nml"), "").unwrap();
        let locked = ws.join("apps/cu/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let bites = matches!(
            std::fs::metadata(locked.join("probe")),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
        );
        if !bites {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return; // root: the lock does not bite
        }
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let index = resolver.index(&ws, &view(&roots));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            index.files,
            vec![
                ws.join("core.model.nml"),
                ws.join("demo.package.nml"),
                ws.join("apps/du/app.nml")
            ],
            "the unit's files are absent, the rest indexed: {:?}",
            index.denials
        );
        assert_eq!(index.denials.len(), 1, "{:?}", index.denials);
        assert!(
            index.denials[0].starts_with(&format!(
                "nothing under `{}` is indexed: [NML2089] discovery under `apps/cu` was cut short",
                ws.join("apps/cu").display()
            )),
            "{}",
            index.denials[0]
        );
    }

    /// Step 0f: under a root a file is named by its key; outside every
    /// root, by its path (an untitled or foreign buffer keys in no
    /// universe).
    #[test]
    fn source_name_is_the_key_under_a_root_and_the_path_outside() {
        let roots = vec![PathBuf::from("/ws"), PathBuf::from("/other")];
        assert_eq!(
            source_name_of(Path::new("/ws/tenants/cu/x.flow.nml"), &roots),
            "tenants/cu/x.flow.nml"
        );
        assert_eq!(source_name_of(Path::new("/other/a.nml"), &roots), "a.nml");
        assert_eq!(
            source_name_of(Path::new("/elsewhere/x.nml"), &roots),
            "/elsewhere/x.nml"
        );
    }

    /// `rootShadowed` on the wire: the shadow spelled from the derived
    /// root it sits above — as many `..` as the root sits below the
    /// shadow's directory — and never absolute.
    #[test]
    fn a_shadow_is_spelled_from_the_root_it_sits_above() {
        assert_eq!(
            shadow_display(Path::new("/a/b/c"), Path::new("/a/.git")),
            "../../.git"
        );
        assert_eq!(
            shadow_display(
                Path::new("/a/b/inner/tenants/cu/flows"),
                Path::new("/a/b/demo.package.nml")
            ),
            "../../../../demo.package.nml"
        );
        assert_eq!(
            shadow_display(Path::new("/a"), Path::new("/a/demo.package.nml")),
            "demo.package.nml",
            "a marker in the root itself is spelled bare"
        );
        assert_eq!(
            shadow_display(Path::new("/x/y"), Path::new("/a/.git")),
            ".git",
            "not above the root: the display form, never absolute"
        );
    }

    #[test]
    fn display_path_is_relative_never_absolute() {
        let roots = vec![PathBuf::from("/ws"), PathBuf::from("/other")];
        assert_eq!(
            display_path(Path::new("/ws/pkg/demo.package.nml"), &roots),
            "pkg/demo.package.nml"
        );
        assert_eq!(
            display_path(
                Path::new("/workspace/demo.package.nml"),
                &[PathBuf::from("/workspace")]
            ),
            "demo.package.nml"
        );
        assert_eq!(display_path(Path::new("/ws"), &roots), "ws");
        assert_eq!(
            display_path(Path::new("/elsewhere/x.package.nml"), &roots),
            "x.package.nml"
        );
    }

    fn covered(outcome: VocabularyOutcome, why: &str) -> VocabularyMatch {
        match outcome {
            VocabularyOutcome::Covered(m) => m,
            VocabularyOutcome::Opaque => panic!("expected coverage ({why}), got Opaque"),
            VocabularyOutcome::Undetermined => {
                panic!("expected coverage ({why}), got Undetermined")
            }
            VocabularyOutcome::Ambiguous { candidates } => {
                panic!("expected coverage ({why}), got Ambiguous({candidates:?})")
            }
        }
    }

    fn test_events() -> (
        tokio::sync::mpsc::Sender<StoreEvent>,
        tokio::sync::mpsc::Receiver<StoreEvent>,
    ) {
        tokio::sync::mpsc::channel(64)
    }

    #[test]
    fn auto_association_binds_store_package_by_marker_root() {
        let ws = temp_ws("auto");
        let store_base = store_dir("auto");
        publish_demo(&Store::at(store_base.to_path_buf()));
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site")).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();

        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let view = view(&roots);
        for rel in ["demo.nml", "apps/site/app.nml"] {
            let resolved = resolver.resolve(&project.join(rel), &view);
            match resolved.resolution {
                Resolution::Bound(b) => {
                    assert_eq!(b.package_name, "demo");
                    assert_eq!(b.step, BindingStep::AutoAssociated);
                    assert_eq!(b.class, ClaimClass::Store);
                    assert_eq!(b.root, project);
                }
                Resolution::Unbound | Resolution::Refused => {
                    panic!("{rel} should bind: {:?}", resolved.notes)
                }
            }
        }
        std::fs::write(project.join("other.nml"), "").unwrap();
        assert!(matches!(
            resolver
                .resolve(&project.join("other.nml"), &view)
                .resolution,
            Resolution::Unbound
        ));
    }

    fn demo_package_versioned(version: &str) -> SchemaPackage {
        let manifest = MANIFEST.replace("version = \"0.1.0\"", &format!("version = \"{version}\""));
        SchemaPackage::from_parts(&manifest, |_| Ok(CORE.to_string())).expect("demo package loads")
    }

    /// A buffer-served discovery input is capped exactly as a disk read
    /// (E39: one verdict, one sentence): a declared source the editor
    /// holds at 4 MiB + 1 fails its manifest (NML2088) in the CLI's own
    /// words, and the claimed file is unbound — never judged under a
    /// package the CLI refuses (it was: NML2004 on every block, blaming
    /// the tenant's content for the operator's oversized source).
    #[test]
    fn a_buffer_served_input_past_its_cap_fails_the_manifest_like_a_disk_read() {
        let ws = temp_ws("buffer-cap");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let source = project.join("core.model.nml");
        let cap = input_cap(InputKind::Source);
        let huge = format!("{CORE}{}", " ".repeat(cap + 1 - CORE.len()));
        let buffers = vec![source.clone()];
        let docs = OneDoc::new(source, &huge);
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let v = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &docs,
        };
        let resolved = resolver.resolve(&project.join("demo.nml"), &v);
        // r86: governed content under a manifest that failed to load is
        // REFUSED (nothing validates), never merely unbound.
        assert!(
            matches!(resolved.resolution, Resolution::Refused),
            "{:?}",
            resolved.notes
        );
        let note = resolved
            .notes
            .iter()
            .find(|n| n.code == Some(nml_core::diagnostic::codes::RESOLUTION_INPUT_UNLOADABLE))
            .unwrap_or_else(|| panic!("no NML2088 row: {:?}", resolved.notes));
        assert!(
            note.message.ends_with(
                "is unavailable: too large: over 4 MiB (4194305 bytes) — a declared schema \
                 source is read only up to 4 MiB (4194304 bytes)"
            ),
            "{}",
            note.message
        );
        // The same source one byte shorter loads, and the file binds (a
        // fresh resolver: the cached universe is keyed by the store's
        // stamp, which a new `OneDoc` at the same path repeats).
        let docs = OneDoc::new(project.join("core.model.nml"), &huge[..cap]);
        let resolver = PackageResolver::new(None, test_events().0);
        let v = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &docs,
        };
        assert!(
            matches!(
                resolver.resolve(&project.join("demo.nml"), &v).resolution,
                Resolution::Bound(_)
            ),
            "exactly the cap reads whole"
        );
    }

    /// The universe cache: a CREATE under root A drops A's universe and
    /// ONLY A's — containment is component-wise (`/ws/a` never swallows
    /// `/ws/ab`); an empty change set retains everything.
    #[test]
    fn create_under_one_root_invalidates_that_root_only() {
        let resolver = PackageResolver::new(None, test_events().0);
        // The guards outlive the test; the cache is keyed by their paths.
        let scratch = [temp_ws("inv-a"), temp_ws("inv-ab"), temp_ws("inv-b")];
        let roots: Vec<PathBuf> = scratch.iter().map(|s| s.to_path_buf()).collect();
        for root in &roots {
            let v = view(std::slice::from_ref(root));
            let _ = resolver.resolve(&root.join("x.nml"), &v);
        }
        assert_eq!(resolver.universes.lock().unwrap().len(), 3);
        resolver.invalidate_claims_for(&[]);
        assert_eq!(
            resolver.universes.lock().unwrap().len(),
            3,
            "empty set retains"
        );
        resolver.invalidate_claims_for(&[roots[0].join("sub").join("new.nml")]);
        let cache = resolver.universes.lock().unwrap();
        assert!(!cache.contains_key(&roots[0]));
        assert!(cache.contains_key(&roots[1]));
        assert!(cache.contains_key(&roots[2]));
        drop(cache);
        resolver.invalidate_claims();
        assert!(resolver.universes.lock().unwrap().is_empty());
        for root in &roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// The OTHER cache a watched-file create or delete has to drop: the
    /// wasm editor's directory listings.
    ///
    /// A universe is a statement about which `.nml` names exist, and a
    /// listing is where that statement comes from — so invalidating the
    /// universes while holding the listings rebuilds them from a listing
    /// that predates the event. Nothing could reach this before: the memo
    /// was compiled for wasi alone and no test lane runs on wasi, so
    /// `forget_listings` could be deleted from both call sites with every
    /// gate green.
    #[test]
    fn a_watched_file_change_forgets_the_listings_memo_and_an_empty_one_does_not() {
        let resolver = PackageResolver::new(None, test_events().0);
        let scratch = temp_ws("listings-inv");
        let root = scratch.to_path_buf();
        std::fs::write(root.join("a.nml"), b"x").unwrap();

        let hold = |r: &PackageResolver| {
            let op = r.listings.snapshot();
            let _ = op(&root).expect("listed");
        };

        hold(&resolver);
        assert_eq!(resolver.held_listings(), 1, "the memo held nothing to drop");
        // An empty change set is not an event: it must retain, or every
        // pull would pay for a full re-walk.
        resolver.invalidate_claims_for(&[]);
        assert_eq!(
            resolver.held_listings(),
            1,
            "an empty change set dropped the memo"
        );

        resolver.invalidate_claims_for(&[root.join("sub").join("new.nml")]);
        assert_eq!(
            resolver.held_listings(),
            0,
            "a watched-file create left the memo holding a listing taken before it"
        );

        hold(&resolver);
        assert_eq!(resolver.held_listings(), 1);
        resolver.invalidate_claims();
        assert_eq!(
            resolver.held_listings(),
            0,
            "the blunt invalidation dropped the universes and kept their listings"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Freshness: a universe is re-discovered when a manifest it read
    /// changes on disk (the generation advances), and served from the
    /// cache otherwise (the generation holds).
    #[test]
    fn universe_is_rediscovered_only_when_an_input_changes() {
        let ws = temp_ws("fresh");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let v = view(&roots);
        let _ = resolver.resolve(&project.join("demo.nml"), &v);
        let g1 = resolver.generation();
        let _ = resolver.resolve(&project.join("demo.nml"), &v);
        assert_eq!(
            resolver.generation(),
            g1,
            "nothing changed: served from the cache"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            project.join("demo.package.nml"),
            MANIFEST.replace("version = \"0.1.0\"", "version = \"0.2.0\""),
        )
        .unwrap();
        match resolver.resolve(&project.join("demo.nml"), &v).resolution {
            Resolution::Bound(b) => assert_eq!(b.package_version, "0.2.0"),
            Resolution::Unbound | Resolution::Refused => panic!("still bound"),
        }
        assert!(
            resolver.generation() > g1,
            "a changed input advances the generation"
        );
    }

    #[test]
    fn watched_path_filter_mirrors_walk_policy() {
        let roots = [PathBuf::from("/home/u/.dotfiles/ws")];
        let root = &roots[0];
        assert!(watched_path_affects_claims(
            &root.join("apps/site/app.nml"),
            &roots
        ));
        assert!(watched_path_affects_claims(&root.join("a.nml"), &roots));
        assert!(watched_path_affects_claims(
            &root.join(".hidden.nml"),
            &roots
        ));
        for skipped in [
            "node_modules/pkg/a.nml",
            "target/debug/a.nml",
            ".git/x.nml",
            "deep/.cache/x.nml",
        ] {
            assert!(
                !watched_path_affects_claims(&root.join(skipped), &roots),
                "{skipped} is invisible to the walk"
            );
        }
        assert!(!watched_path_affects_claims(
            &root.join("README.md"),
            &roots
        ));
        assert!(!watched_path_affects_claims(
            Path::new("/elsewhere/x.nml"),
            &roots
        ));
    }

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
        let roots = vec![ws.to_path_buf()];
        match resolver
            .resolve(&project.join("demo.nml"), &view(&roots))
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(b.package_name, "demo");
                assert_eq!(b.class, ClaimClass::Injected);
                assert_eq!(b.package_version, "9.9.9");
                assert_eq!(b.root, project);
            }
            Resolution::Unbound | Resolution::Refused => {
                panic!("injected package should bind demo.nml")
            }
        }
    }

    #[test]
    fn injected_beats_store_for_same_name() {
        let ws = temp_ws("inj-store");
        let store_base = store_dir("inj-store");
        publish_demo(&Store::at(store_base.to_path_buf()));
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();

        let resolver = PackageResolver::with_injected(
            Some(Store::at(store_base.to_path_buf())),
            test_events().0,
            Some(demo_package_versioned("9.9.9")),
        );
        let roots = vec![ws.to_path_buf()];
        match resolver
            .resolve(&project.join("demo.nml"), &view(&roots))
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(b.class, ClaimClass::Injected, "in-binary beats cache");
                assert_eq!(b.package_version, "9.9.9");
            }
            Resolution::Unbound | Resolution::Refused => panic!("should bind"),
        }
    }

    #[test]
    fn workspace_manifest_beats_injected_for_same_name() {
        let ws = temp_ws("inj-ws");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        let manifest_text = MANIFEST.replace("version = \"0.1.0\"", "version = \"2.0.0\"");
        std::fs::write(project.join("demo.package.nml"), &manifest_text).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();

        let resolver = PackageResolver::with_injected(
            None,
            test_events().0,
            Some(demo_package_versioned("9.9.9")),
        );
        let roots = vec![ws.to_path_buf()];
        match resolver
            .resolve(&project.join("demo.nml"), &view(&roots))
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(
                    b.class,
                    ClaimClass::Workspace,
                    "committed manifest beats in-binary"
                );
                assert_eq!(b.package_version, "2.0.0");
            }
            Resolution::Unbound | Resolution::Refused => panic!("should bind"),
        }
    }

    #[test]
    fn opt_out_disables_auto_association_and_pin_restores() {
        let ws = temp_ws("optout");
        let store_base = store_dir("optout");
        publish_demo(&Store::at(store_base.to_path_buf()));
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    autoAssociate = false\n",
        )
        .unwrap();

        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        assert!(matches!(
            resolver
                .resolve(&project.join("demo.nml"), &view(&roots))
                .resolution,
            Resolution::Unbound
        ));

        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    autoAssociate = false\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        match resolver
            .resolve(&project.join("demo.nml"), &view(&roots))
            .resolution
        {
            Resolution::Bound(b) => assert_eq!(b.step, BindingStep::Pinned),
            Resolution::Unbound | Resolution::Refused => panic!("pin must bind"),
        }
    }

    #[test]
    fn workspace_manifest_shadows_store_with_visible_note() {
        let ws = temp_ws("shadow");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(store_base.to_path_buf()));
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

        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("demo.nml"), &view(&roots));
        match resolved.resolution {
            Resolution::Bound(b) => {
                assert_eq!(b.class, ClaimClass::Workspace);
                assert!(b.shadows_store);
            }
            Resolution::Unbound | Resolution::Refused => panic!("must bind"),
        }
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("shadowing")),
            "{:?}",
            resolved.notes
        );
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
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("whatever.nml"), &view(&roots));
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("'ghost' is not installed")),
            "{:?}",
            resolved.notes
        );
    }

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
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("x.nml"), &view(&roots));
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("not a valid package name")),
            "{:?}",
            resolved.notes
        );
    }

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
        let roots = vec![ws.to_path_buf()];
        assert!(matches!(
            resolver
                .resolve(&project.join("demo.nml"), &view(&roots))
                .resolution,
            Resolution::Unbound
        ));
        std::fs::write(vendored.join("demo.nml"), "").unwrap();
        resolver.invalidate_claims();
        assert!(matches!(
            resolver
                .resolve(&vendored.join("demo.nml"), &view(&roots))
                .resolution,
            Resolution::Bound(_)
        ));
    }

    /// RFC 0030's stem rule, now the kernel's (E35): a manifest whose file
    /// stem differs from its declared name is a LOAD ERROR that closes the
    /// universe — surfaced on the manifest document as a note, and on
    /// every file under the root — never a second package.
    #[test]
    fn stem_name_mismatch_is_a_load_error_note() {
        let ws = temp_ws("stem");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let mismatched = MANIFEST.replace("package demo:", "package other:");
        let manifest_path = project.join("demo.package.nml");
        std::fs::write(&manifest_path, &mismatched).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&manifest_path, &view(&roots));
        assert!(
            matches!(resolved.resolution, Resolution::Bound(ref b) if b.class == ClaimClass::Builtin),
            "the manifest itself still validates under the builtin meta package"
        );
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("declares name `other`")
                    && n.message.contains("expected `other.package.nml`")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn store_format_version_gate_uses_contract_wording() {
        let ws = temp_ws("storefv");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        let future = MANIFEST.replace("formatVersion = 1", "formatVersion = 99");
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
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("x.nml"), &view(&roots));
        assert!(
            resolved
                .notes
                .iter()
                .any(|n| n.message.contains("needs formatVersion 99")
                    && n.message.contains("update nml-lsp")),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn broken_store_package_is_quiet_for_unpinned_files() {
        let ws = temp_ws("quietcorrupt");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(store_base.to_path_buf()));
        let pkg_dir = store_base.join("schema-packages/demo");
        std::fs::write(pkg_dir.join("current"), "0.1.0+badbadba\nblake3:wrong\n").unwrap();
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("unrelated.nml"), "").unwrap();
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("unrelated.nml"), &view(&roots));
        assert!(matches!(resolved.resolution, Resolution::Unbound));
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
    }

    #[test]
    fn store_failure_transition_is_pushed_to_the_channel() {
        let ws = temp_ws("eventpush");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        publish_demo(&Store::at(store_base.to_path_buf()));
        let pkg_dir = store_base.join("schema-packages/demo");
        std::fs::write(pkg_dir.join("current"), "0.1.0+bad00000\nblake3:wrong\n").unwrap();
        let (tx, mut rx) = test_events();
        let resolver = PackageResolver::new(Some(Store::at(store_base.to_path_buf())), tx);
        std::fs::create_dir_all(ws.join("proj")).unwrap();
        std::fs::write(
            ws.join("proj/nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        std::fs::write(ws.join("proj/x.nml"), "").unwrap();
        let roots = vec![ws.to_path_buf()];
        let _ = resolver.resolve(&ws.join("proj/x.nml"), &view(&roots));
        let ev = rx.try_recv().expect("failure transition pushed");
        assert!(
            ev.warning && ev.message.contains("failed to load"),
            "{ev:?}"
        );
        assert!(
            rx.try_recv().is_err(),
            "one-shot: no duplicate on same state"
        );
        let _ = resolver.resolve(&ws.join("proj/x.nml"), &view(&roots));
        assert!(rx.try_recv().is_err(), "cached outcome pushes nothing");
    }

    #[test]
    fn overflow_drops_newest_events() {
        let ws = temp_ws("overflow");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        let _ = publish_demo(&Store::at(store_base.to_path_buf()));
        let pointer_path = store_base.join("schema-packages/demo/current");
        let valid = std::fs::read_to_string(&pointer_path).unwrap();
        let corrupt = "0.1.0+bad00000\nblake3:wrong\n";

        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("nml-project.nml"),
            "project P:\n    schemaPackages:\n        - demo\n",
        )
        .unwrap();
        std::fs::write(project.join("x.nml"), "").unwrap();

        let (tx, mut rx) = test_events();
        let resolver = PackageResolver::new(Some(Store::at(store_base.to_path_buf())), tx);
        let roots = vec![ws.to_path_buf()];
        for i in 0..70 {
            let content = if i % 2 == 0 { corrupt } else { valid.as_str() };
            std::fs::write(&pointer_path, content).unwrap();
            let _ = resolver.resolve(&project.join("x.nml"), &view(&roots));
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
    }

    /// The editor's leaf read never blocks on the open and never reads
    /// a non-regular file: a FIFO named like an input — planted, or
    /// swapped in for a file between the walk's `lstat` and the read —
    /// is refused in the kernel's sentence within the moment, where a
    /// plain `File::open` parked the server thread inside `open(2)`
    /// until a writer appeared (never), every request after it timing
    /// out. A directory is refused the same way; a regular file reads.
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_an_input_is_refused_not_blocked_on() {
        let ws = temp_ws("fifo-input");
        let fifo = ws.join("core.model.nml");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo");
        std::fs::write(ws.join("plain.model.nml"), "model core:\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = ws.to_path_buf();
        std::thread::spawn(move || {
            let fifo = read_input_at_leaf(InputKind::Source, &dir.join("core.model.nml"));
            let directory = read_input_at_leaf(InputKind::Source, &dir);
            let plain = read_input_at_leaf(InputKind::Source, &dir.join("plain.model.nml"));
            let _ = tx.send((fifo, directory, plain));
        });
        let (fifo, directory, plain) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read returned — a FIFO must never block the open");
        let fifo = fifo.expect_err("a FIFO is refused");
        assert!(
            fifo.contains("`core.model.nml` is not a regular file (refused at open)"),
            "{fifo}"
        );
        let directory = directory.expect_err("a directory is refused");
        assert!(directory.contains("Is a directory"), "{directory}");
        assert_eq!(plain.expect("a regular file reads"), "model core:\n");
    }

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
        let roots = vec![ws.to_path_buf()];
        let vocab = covered(
            resolver.vocabulary_for(&project.join("core.model.nml"), &view(&roots)),
            "declared source is covered",
        );
        assert!(!vocab.undeclared_sibling);
        assert_eq!(vocab.vocabulary.package_name(), "demo");
        let names: Vec<&str> = vocab
            .vocabulary
            .declared()
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["live", "restart", "key"]);
        match &vocab.universe {
            SchemaUniverse::Declared(files) => assert_eq!(
                files,
                &vec![project.join("core.model.nml")],
                "declared []schema entries resolve against the manifest dir"
            ),
            other => panic!("workspace coverage must declare paths, got {other:?}"),
        }
    }

    #[test]
    fn store_slot_paths_resolve_uncovered() {
        let ws = temp_ws("outside-roots-uncovered");
        let store_base = ws.join("store");
        std::fs::create_dir_all(&store_base).unwrap();
        Store::at(store_base.to_path_buf())
            .publish(&nml_validate::test_support::demo_package_with_directives())
            .expect("publish");
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.join("project")];
        std::fs::create_dir_all(&roots[0]).unwrap();
        let outside = store_base.join("schema-packages/demo/core.model.nml");
        assert!(
            !matches!(
                resolver.vocabulary_for(&outside, &view(&roots)),
                VocabularyOutcome::Covered(_)
            ),
            "outside-roots paths must not resolve coverage"
        );
    }

    #[test]
    fn vocabulary_for_root_coverage_and_sibling_flag() {
        use nml_validate::test_support::{DEMO_CORE, DEMO_MANIFEST_WITH_DIRECTIVES};
        let ws = temp_ws("vocab-root");
        let store_base = store_dir("vocab-root");
        let store = Store::at(store_base.to_path_buf());
        store
            .publish(&nml_validate::test_support::demo_package_with_directives())
            .expect("publish");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("schemas")).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("schemas/extra.model.nml"), DEMO_CORE).unwrap();
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let vocab = covered(
            resolver.vocabulary_for(&project.join("schemas/extra.model.nml"), &view(&roots)),
            "root coverage applies",
        );
        assert!(!vocab.undeclared_sibling, "store coverage is not a sibling");
        assert_eq!(vocab.vocabulary.package_name(), "demo");
        match &vocab.universe {
            SchemaUniverse::Snapshot(pkg) => assert_eq!(pkg.sources.len(), 1),
            other => panic!("store coverage must snapshot the package, got {other:?}"),
        }

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
        resolver.invalidate_claims();
        let vocab = covered(
            resolver.vocabulary_for(&wsproj.join("stray.model.nml"), &view(&roots)),
            "sibling is covered by the root rule",
        );
        assert!(vocab.undeclared_sibling);
        match &vocab.universe {
            SchemaUniverse::Declared(files) => assert_eq!(
                files,
                &vec![wsproj.join("core.model.nml")],
                "workspace root-coverage resolves []schema against the manifest dir"
            ),
            other => panic!("workspace root-coverage must declare paths, got {other:?}"),
        }
    }

    /// The editor's coverage question has no cap of its own any more:
    /// 2,100 filler entries (which capped the pre-0e claims walk at 2,048
    /// and left the answer `Undetermined` forever) are enumerated by the
    /// kernel's one walk and the bound file behind them is seen.
    #[test]
    fn root_coverage_survives_a_wide_root() {
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
        for i in 0..2100 {
            std::fs::write(project.join(format!("filler-{i}.txt")), "").unwrap();
        }
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let vocab = covered(
            resolver.vocabulary_for(&project.join("stray.model.nml"), &view(&roots)),
            "the bound file behind the fillers is seen",
        );
        assert_eq!(vocab.vocabulary.package_name(), "demo");
    }

    #[test]
    fn vocabulary_for_uncovered_file_is_opaque() {
        let ws = temp_ws("vocab-opaque");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("lonely.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        assert!(matches!(
            resolver.vocabulary_for(&project.join("lonely.model.nml"), &view(&roots)),
            VocabularyOutcome::Opaque
        ));
    }

    #[test]
    fn builtin_binds_package_manifests_anywhere() {
        let ws = temp_ws("builtin");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let manifest_path = project.join("demo.package.nml");
        std::fs::write(&manifest_path, MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        match resolver.resolve(&manifest_path, &view(&roots)).resolution {
            Resolution::Bound(b) => {
                assert_eq!(b.package_name, "nml");
                assert_eq!(b.class, ClaimClass::Builtin);
            }
            Resolution::Unbound | Resolution::Refused => {
                panic!("manifest must bind to builtin meta package")
            }
        }
    }

    /// Step 0e's capability gain: an UNSAVED manifest buffer at a path the
    /// disk lacks is a live resolution input — the kernel walks the
    /// overlay, reads the buffer, and binds the file.
    #[test]
    fn unsaved_manifest_buffer_binds_through_the_overlay() {
        let ws = temp_ws("overlay");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let manifest_path = project.join("demo.package.nml");
        let buffers = vec![manifest_path.clone()];
        let docs = OneDoc::new(manifest_path.clone(), MANIFEST);
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let v = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &docs,
        };
        match resolver.resolve(&project.join("demo.nml"), &v).resolution {
            Resolution::Bound(b) => {
                assert_eq!(b.class, ClaimClass::Workspace);
                assert_eq!(b.manifest.as_deref(), Some(manifest_path.as_path()));
            }
            Resolution::Unbound | Resolution::Refused => panic!("the unsaved manifest must bind"),
        }
    }

    /// The universe's freshness guard reads one STAMP per stored-document
    /// read, never the text: after the walk, a pull that changes nothing
    /// reads no text at all and is served from the cache; a text written
    /// under the same stamp is invisible (the store never writes one
    /// without a new stamp); a new stamp rediscovers.
    #[test]
    fn universe_freshness_reads_stamps_not_texts() {
        let ws = temp_ws("stamps");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let manifest_path = project.join("demo.package.nml");
        let buffers = vec![manifest_path.clone()];
        let docs = OneDoc::new(manifest_path, MANIFEST);
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let v = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &docs,
        };
        let version = |r: Resolved| match r.resolution {
            Resolution::Bound(b) => b.package_version,
            Resolution::Unbound | Resolution::Refused => {
                panic!("the buffered manifest binds: {:?}", r.notes)
            }
        };
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &v)),
            "0.1.0"
        );
        assert_eq!(docs.texts_read.get(), 1, "the discovery read");
        let g1 = resolver.generation();
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &v)),
            "0.1.0"
        );
        assert_eq!(docs.texts_read.get(), 1, "a fresh universe reads no text");
        assert_eq!(resolver.generation(), g1);
        *docs.text.borrow_mut() = MANIFEST.replace("version = \"0.1.0\"", "version = \"0.2.0\"");
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &v)),
            "0.1.0",
            "the same stamp is the same document"
        );
        assert_eq!(docs.texts_read.get(), 1);
        docs.stamp.set(2);
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &v)),
            "0.2.0",
            "a new stamp is a new text: rediscovered"
        );
        assert_eq!(docs.texts_read.get(), 2);
        assert!(resolver.generation() > g1);
    }

    /// r89 (composition mutant C7 survived): every discovery — a folder's
    /// or a derived root's — hands out the resolver's ONE validator table
    /// (P1's promise: a rediscovered manifest with an unchanged hash costs
    /// no second build). A discovery that kept the kernel's fresh table
    /// passed every pin.
    #[test]
    fn every_discovery_shares_the_resolvers_validator_table() {
        let ws = temp_ws("shared-memo");
        let store_base = store_dir("shared-memo");
        publish_demo(&Store::at(store_base.to_path_buf()));
        let inside = ws.join("proj");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("demo.nml"), "").unwrap();
        let elsewhere = temp_ws("shared-memo-elsewhere");
        std::fs::write(elsewhere.join("demo.nml"), "").unwrap();
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let view = view(&roots);
        resolver.resolve(&inside.join("demo.nml"), &view);
        // A derived root's universe too — unless a checkout above the
        // scratch dir would make the fence its own (the folder's suffices).
        let derived = !elsewhere.ancestors().any(|d| d.join(".git").exists());
        if derived {
            resolver.resolve(&elsewhere.join("demo.nml"), &view);
        }
        let universes = resolver.universes.lock().unwrap();
        assert_eq!(universes.len(), if derived { 2 } else { 1 });
        for (root, cached) in universes.iter() {
            assert!(
                Arc::ptr_eq(cached.discovery.validators(), &resolver.validators),
                "{}: a discovery with a table of its own",
                root.display()
            );
        }
        drop(universes);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// R1's third rung (r88 P2; step 0e's first delta E37 amended): a
    /// file outside every workspace FOLDER resolves under the root the
    /// KERNEL derives — with no `.git` above, its own directory (E21's
    /// no-VCS fence) — exactly as `nml check <file>` resolves it: the
    /// store's demo package auto-associates by its marker there, as it
    /// does inside a folder. What E37 forbade stays forbidden: the
    /// pre-0e editor anchored store globs at the file's own directory
    /// UNCONDITIONALLY; the kernel's derivation is fenced — a marker in
    /// the directory ABOVE a no-VCS target's own directory is outside
    /// the fence and never re-roots the file, which stays unbound.
    #[test]
    fn a_file_outside_every_workspace_folder_resolves_under_the_kernels_derived_root() {
        let ws = temp_ws("outside");
        let store_base = store_dir("outside");
        publish_demo(&Store::at(store_base.to_path_buf()));
        let inside = ws.join("proj");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("demo.nml"), "").unwrap();
        let elsewhere = temp_ws("outside-elsewhere");
        if elsewhere.ancestors().any(|d| d.join(".git").exists()) {
            return; // a checkout above the scratch dir: the fence would be its own
        }
        std::fs::write(elsewhere.join("demo.nml"), "").unwrap();
        std::fs::create_dir_all(elsewhere.join("sub")).unwrap();
        std::fs::write(elsewhere.join("sub/x.nml"), "").unwrap();
        assert!(!elsewhere.starts_with(&ws));
        let resolver =
            PackageResolver::new(Some(Store::at(store_base.to_path_buf())), test_events().0);
        let roots = vec![ws.to_path_buf()];
        let view = view(&roots);
        assert!(
            matches!(
                resolver.resolve(&inside.join("demo.nml"), &view).resolution,
                Resolution::Bound(_)
            ),
            "inside the folder the marker file auto-associates"
        );
        let outside = resolver.resolve(&elsewhere.join("demo.nml"), &view);
        assert!(
            matches!(outside.resolution, Resolution::Bound(_)),
            "the marker file auto-associates under the derived root, as for the CLI: {:?}",
            outside.notes
        );
        let (root, origin) = outside.root.clone().expect("a derived root");
        assert_eq!(root, dunce::canonicalize(&elsewhere).unwrap());
        assert_eq!(origin.tag(), "derivedTargetDir");
        assert!(outside.notes.is_empty(), "{:?}", outside.notes);
        // The marker sits ABOVE `sub`, outside the no-VCS fence: no re-rooting.
        let fenced = resolver.resolve(&elsewhere.join("sub/x.nml"), &view);
        assert!(
            matches!(fenced.resolution, Resolution::Unbound),
            "{:?}",
            fenced.notes
        );
        assert_eq!(
            fenced.root.map(|(r, _)| r),
            Some(dunce::canonicalize(elsewhere.join("sub")).unwrap()),
            "the target's own directory is the universe"
        );
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// The index is the kernel's enumeration: what the walk skips by
    /// policy (`target/`, `node_modules/`, a dot-directory), a symlink —
    /// one whose target is inside the root included — and a dot-file are
    /// not in it; a source-dir schema is, and so is a file behind two
    /// thousand fillers (the index has no cap of its own). A root the
    /// walk enumerates in full denies nothing.
    #[test]
    fn the_index_is_the_kernels_enumeration() {
        let ws = temp_ws("index");
        for dir in [
            "target/package/x",
            "node_modules/pkg",
            ".cache",
            "src",
            "fill",
        ] {
            std::fs::create_dir_all(ws.join(dir)).unwrap();
        }
        std::fs::write(ws.join("target/package/x/a.model.nml"), "model a:\n").unwrap();
        std::fs::write(ws.join("node_modules/pkg/b.model.nml"), "model b:\n").unwrap();
        std::fs::write(ws.join(".cache/c.model.nml"), "model c:\n").unwrap();
        std::fs::write(ws.join("src/d.model.nml"), "model d:\n").unwrap();
        std::fs::write(ws.join("src/.hidden.model.nml"), "model h:\n").unwrap();
        std::fs::write(ws.join("e.nml"), "").unwrap();
        std::fs::write(ws.join("notes.txt"), "").unwrap();
        for i in 0..2100 {
            std::fs::write(ws.join(format!("fill/f-{i}.txt")), "").unwrap();
        }
        std::fs::write(ws.join("fill/behind.nml"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(ws.join("src/d.model.nml"), ws.join("src/link.model.nml"))
            .unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let index = resolver.index(&ws, &view(&roots));
        let mut files = index.files;
        files.sort();
        assert_eq!(
            files,
            vec![
                ws.join("e.nml"),
                ws.join("fill/behind.nml"),
                ws.join("src/d.model.nml")
            ]
        );
        assert!(index.denials.is_empty(), "{:?}", index.denials);
    }

    /// A root the walk cannot enumerate indexes NOTHING and says so —
    /// fail-closed, where the old index walk silently skipped a directory
    /// it could not list — and heals on the next pull after the directory
    /// is listable (the stop directory is fingerprinted like a read; no
    /// watched event names a `chmod`); a live manifest that fails to load
    /// is named too, its files indexed (the walk completed) and every one
    /// unbound.
    #[cfg(unix)]
    #[test]
    fn the_index_denies_what_the_kernel_denies_and_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let ws = temp_ws("index-denied");
        std::fs::write(ws.join("ok.model.nml"), "model okmodel:\n    a number\n").unwrap();
        let locked = ws.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let bites = matches!(
            std::fs::metadata(locked.join("probe")),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
        );
        if !bites {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return; // root: the lock does not bite
        }
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let index = resolver.index(&ws, &view(&roots));
        assert!(index.files.is_empty(), "{:?}", index.files);
        assert_eq!(index.denials.len(), 1, "{:?}", index.denials);
        let denial = &index.denials[0];
        assert!(
            denial.starts_with(&format!(
                "nothing under `{}` is indexed: [NML2089] ",
                ws.display()
            )) && denial.contains("the walk stopped at `locked` (unreadable:"),
            "{denial}"
        );
        let g1 = resolver.generation();
        let again = resolver.index(&ws, &view(&roots));
        assert_eq!(
            again.denials, index.denials,
            "unchanged: served from the cache"
        );
        assert_eq!(resolver.generation(), g1);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let healed = resolver.index(&ws, &view(&roots));
        assert_eq!(
            healed.files,
            vec![ws.join("ok.model.nml")],
            "{:?}",
            healed.denials
        );
        assert!(healed.denials.is_empty(), "{:?}", healed.denials);
        assert!(
            resolver.generation() > g1,
            "the listable directory rediscovers"
        );
        // A manifest declaring an absent source: NML2088, files indexed.
        // A created file is a watched event, not a read: the editor's
        // watcher invalidates the root.
        std::fs::write(ws.join("demo.package.nml"), MANIFEST).unwrap();
        resolver.invalidate_claims_for(&[ws.join("demo.package.nml")]);
        let index = resolver.index(&ws, &view(&roots));
        assert_eq!(
            index.files,
            vec![ws.join("demo.package.nml"), ws.join("ok.model.nml")]
        );
        assert_eq!(index.denials.len(), 1, "{:?}", index.denials);
        assert!(
            index.denials[0].starts_with("[NML2088] manifest failed to load: declared source"),
            "{}",
            index.denials[0]
        );
    }

    /// The tenant re-rooting attack the pre-0e editor was open to (its
    /// nearest-ancestor `nml-project.nml` walk): a tenant-committed config
    /// with `autoAssociate = false` inside content the operator's binding
    /// claims is INERT — the file stays bound, and the note says so.
    /// Step 0e: `resolve()` carries the universe's composition grant for
    /// the file — the kernel's own `Grant`, owned: an unclaimed
    /// file in a closed universe is `Unbound { closed: Some((root, n)) }`
    /// (NML2064's closed form), a claimed file carries its binding's
    /// grant or the no-grant denial, and outside every root the context
    /// is open.
    #[test]
    fn resolve_carries_the_universes_grant_for_the_file() {
        let ws = temp_ws("grant");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site")).unwrap();
        std::fs::create_dir_all(project.join("docs")).unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();
        std::fs::write(project.join("docs/unclaimed.nml"), "").unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let unclaimed = resolver.resolve(&project.join("docs/unclaimed.nml"), &view(&roots));
        assert!(matches!(unclaimed.resolution, Resolution::Unbound));
        match &unclaimed.grant {
            Grant::Unbound {
                closed: Some(claims),
            } => assert!(*claims >= 1, "{claims}"),
            other => panic!("{other:?}"),
        }
        let bound = resolver.resolve(&project.join("apps/site/app.nml"), &view(&roots));
        assert!(
            matches!(bound.resolution, Resolution::Bound(_)),
            "{:?}",
            bound.notes
        );
        assert_eq!(
            bound.key.as_ref().map(|k| k.as_str()),
            Some("proj/apps/site/app.nml"),
            "the kernel's key rides the resolution: the name every finding carries"
        );
        assert!(
            matches!(bound.grant, Grant::NoGrant { .. } | Grant::Granted { .. }),
            "{:?}",
            bound.grant
        );
        // The guard is bound (an inline `temp_ws(..).join(..)` drops the
        // directory at the end of its own statement).
        let elsewhere = temp_ws("grant-outside");
        let outside = elsewhere.join("x.nml");
        std::fs::write(&outside, "").unwrap();
        let out = resolver.resolve(&outside, &view(&roots));
        assert_eq!(
            out.grant,
            Grant::open(),
            "an open universe: no manifest within the fence"
        );
        // Outside every folder the kernel derives a root (r88 P2): the
        // key is minted under it, and the root rides the resolution.
        assert!(out.key.is_some(), "a key under the derived root");
        assert!(
            out.root
                .as_ref()
                .is_some_and(|(_, origin)| origin.tag().starts_with("derived")),
            "{:?}",
            out.root
        );
    }

    /// r89 (P11): NML2092 lands on the MANIFEST document, at the glob
    /// that delegates shallower than its inferred unit — never on the
    /// tenant file beneath it — and an explicit `budgetUnits` silences it.
    #[test]
    fn the_gap_lint_lands_on_the_manifest_at_the_glob() {
        let ws = temp_ws("gap-lint");
        let loud = MANIFEST.replace("\"apps/*/app.nml\"", "\"apps/*/flows/**\"");
        std::fs::create_dir_all(ws.join("apps/site/flows")).unwrap();
        std::fs::write(ws.join("demo.package.nml"), &loud).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        std::fs::write(ws.join("apps/site/flows/x.nml"), "").unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let gap = |n: &DegradedNote| n.code == Some(nml_core::diagnostic::codes::BUDGET_UNIT_GAP);
        let file = resolver.resolve(&ws.join("apps/site/flows/x.nml"), &view(&roots));
        assert!(
            matches!(file.resolution, Resolution::Bound(_)),
            "{:?}",
            file.notes
        );
        assert!(!file.notes.iter().any(gap), "{:?}", file.notes);
        let own = resolver.resolve(&ws.join("demo.package.nml"), &view(&roots));
        let notes: Vec<&DegradedNote> = own.notes.iter().filter(|n| gap(n)).collect();
        assert_eq!(notes.len(), 1, "{:?}", own.notes);
        assert_eq!(notes[0].severity, Severity::Warning);
        let NoteAnchor::At(span) = notes[0].anchor else {
            panic!("anchored at the glob: {:?}", notes[0].anchor);
        };
        assert_eq!(&loud[span.start..span.end], "\"apps/*/flows/**\"");
        assert!(
            notes[0]
                .message
                .contains("declare budgetUnits = [\"apps/*\"]"),
            "{}",
            notes[0].message
        );
        // Declared: silent.
        let declared = loud.replace(
            "    formatVersion = 1\n",
            "    formatVersion = 1\n    budgetUnits:\n        - \"apps/*\"\n",
        );
        std::fs::write(ws.join("demo.package.nml"), &declared).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let own = resolver.resolve(&ws.join("demo.package.nml"), &view(&roots));
        assert!(!own.notes.iter().any(gap), "{:?}", own.notes);
    }

    /// RFC 0026 B-5, both front ends: the unit-layout lint rides a
    /// universe that STANDS. Under a live manifest that failed to load
    /// (NML2088) the manifest document carries the universe's error and
    /// NO layout note — exactly as `nml check` prints none — because the
    /// rule is the kernel's (`Discovery::universe_notes`), read here, not
    /// re-spelled over `workspace::budget_unit_gaps()`.
    #[test]
    fn the_gap_lint_is_silent_under_a_broken_universe() {
        let ws = temp_ws("gap-lint-broken");
        let loud = MANIFEST.replace("\"apps/*/app.nml\"", "\"apps/*/flows/**\"");
        std::fs::create_dir_all(ws.join("apps/site/flows")).unwrap();
        std::fs::write(ws.join("demo.package.nml"), &loud).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        std::fs::write(ws.join("apps/site/flows/x.nml"), "").unwrap();
        std::fs::write(
            ws.join("other.package.nml"),
            "package other:\n    version = \"0.1.0\"\n",
        )
        .unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let own = resolver.resolve(&ws.join("demo.package.nml"), &view(&roots));
        assert!(
            own.notes.iter().any(|n| {
                n.code == Some(nml_core::diagnostic::codes::RESOLUTION_INPUT_UNLOADABLE)
                    // Located in the OTHER manifest: its place travels as a
                    // related location there (the sentence names no file).
                    && n.related
                        .iter()
                        .any(|r| r.source.as_deref() == Some("other.package.nml"))
            }),
            "the universe's error rides the manifest document: {:?}",
            own.notes
        );
        assert!(
            !own.notes
                .iter()
                .any(|n| n.code == Some(nml_core::diagnostic::codes::BUDGET_UNIT_GAP)),
            "the lint is silent under a broken universe: {:?}",
            own.notes
        );
    }

    /// RFC 0026 B-1 (NML2081): a `layers:` grant breaking a loader rule
    /// fails the manifest at LOAD — the content file it governs is
    /// REFUSED with the one NML2081 row (the universe is closed-denied
    /// around the manifest, as `nml check` exits 1 before any target),
    /// and on the MANIFEST document the row is anchored AT THE ITEM (the
    /// offending glob), as the layout lint is — never at 1:1.
    #[test]
    fn a_grant_breaking_its_rules_refuses_the_content_and_lands_at_the_item() {
        let ws = temp_ws("grant-rule");
        let bad = MANIFEST.replace(
            "        strict = true\n",
            "        strict = true\n        layers:\n            allowRefs:\n                - \"vendor/**x\"\n",
        );
        assert_ne!(bad, MANIFEST);
        std::fs::create_dir_all(ws.join("apps/site")).unwrap();
        std::fs::write(ws.join("demo.package.nml"), &bad).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        std::fs::write(ws.join("apps/site/app.nml"), "").unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let rule = |n: &DegradedNote| n.code == Some(nml_core::diagnostic::codes::LAYER_GRANT_RULE);
        let file = resolver.resolve(&ws.join("apps/site/app.nml"), &view(&roots));
        assert!(
            matches!(file.resolution, Resolution::Refused),
            "{:?}",
            file.notes
        );
        let rows: Vec<&DegradedNote> = file.notes.iter().filter(|n| rule(n)).collect();
        assert_eq!(rows.len(), 1, "{:?}", file.notes);
        assert_eq!(rows[0].severity, Severity::Error);
        assert!(
            matches!(rows[0].anchor, NoteAnchor::Top),
            "the content file: the document as a whole"
        );
        let own = resolver.resolve(&ws.join("demo.package.nml"), &view(&roots));
        let rows: Vec<&DegradedNote> = own.notes.iter().filter(|n| rule(n)).collect();
        assert_eq!(rows.len(), 1, "{:?}", own.notes);
        let NoteAnchor::At(span) = rows[0].anchor else {
            panic!("anchored at the item: {:?}", rows[0].anchor);
        };
        assert_eq!(&bad[span.start..span.end], "\"vendor/**x\"");
        assert!(
            rows[0].message.contains("`**` must be a whole segment"),
            "{}",
            rows[0].message
        );
    }

    /// RFC 0026 B-3: the NESTED form of the gap lint (an inferred unit
    /// nesting inside another glob's the multiplying way) lands on the
    /// manifest at the INNER glob, naming the outer unit and the one
    /// declaration the loader accepts — the kernel's sentence, as
    /// `nml check` prints it.
    #[test]
    fn the_nested_gap_form_lands_on_the_manifest_at_the_inner_glob() {
        let ws = temp_ws("gap-nested");
        let nested = MANIFEST.replace(
            "            - \"apps/*/app.nml\"\n",
            "            - \"apps/**/*.flow.nml\"\n            - \"apps/*/plugins/*/**/*.model.nml\"\n",
        );
        assert_ne!(nested, MANIFEST);
        std::fs::write(ws.join("demo.package.nml"), &nested).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let gap = |n: &DegradedNote| n.code == Some(nml_core::diagnostic::codes::BUDGET_UNIT_GAP);
        let own = resolver.resolve(&ws.join("demo.package.nml"), &view(&roots));
        let notes: Vec<&DegradedNote> = own.notes.iter().filter(|n| gap(n)).collect();
        assert_eq!(notes.len(), 1, "{:?}", own.notes);
        let NoteAnchor::At(span) = notes[0].anchor else {
            panic!("anchored at the inner glob: {:?}", notes[0].anchor);
        };
        assert_eq!(
            &nested[span.start..span.end],
            "\"apps/*/plugins/*/**/*.model.nml\""
        );
        let message = &notes[0].message;
        assert!(
            message.contains("nests inside `apps/*`")
                && message.contains("declare budgetUnits = [\"apps/*\"]")
                && !message.contains("or ["),
            "{message}"
        );
    }

    #[test]
    fn inert_tenant_config_cannot_unbind_the_operators_file() {
        let ws = temp_ws("inert");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site")).unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();
        std::fs::write(
            project.join("apps/site/nml-project.nml"),
            "project P:\n    autoAssociate = false\n",
        )
        .unwrap();
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let resolved = resolver.resolve(&project.join("apps/site/app.nml"), &view(&roots));
        assert!(
            matches!(resolved.resolution, Resolution::Bound(_)),
            "{:?}",
            resolved.notes
        );
        // r85 D6: the inert note is NOT on the file beneath the input —
        // it is on the input's OWN document, once, as information, at
        // its declaration.
        let inert =
            |n: &DegradedNote| n.code == Some(nml_core::diagnostic::codes::INERT_RESOLUTION_INPUT);
        assert!(
            !resolved.notes.iter().any(inert),
            "the inert note rode the file beneath the input: {:?}",
            resolved.notes
        );
        let own = resolver.resolve(&project.join("apps/site/nml-project.nml"), &view(&roots));
        let notes: Vec<&DegradedNote> = own.notes.iter().filter(|n| inert(n)).collect();
        assert_eq!(notes.len(), 1, "{:?}", own.notes);
        assert_eq!(notes[0].severity, Severity::Info);
        assert_eq!(notes[0].anchor, NoteAnchor::Declaration);
        assert!(
            notes[0]
                .message
                .starts_with("project config `proj/apps/site/nml-project.nml` is inert:"),
            "{}",
            notes[0].message
        );
    }

    /// r85 D7: a file the walk could not finish for — the whole universe
    /// truncated, or the budget unit the file sits under denied — is
    /// `Resolution::Refused`: the notes carry the NML2089 row and NOTHING
    /// validates, as `nml check` validates nothing and exits 1; the
    /// sibling unit resolves as ever.
    #[cfg(unix)]
    #[test]
    fn a_walk_that_did_not_finish_refuses_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let ws = temp_ws("refused");
        let project = ws.join("proj");
        std::fs::create_dir_all(project.join("apps/site/locked")).unwrap();
        std::fs::create_dir_all(project.join("apps/other")).unwrap();
        std::fs::write(project.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        std::fs::write(project.join("apps/site/app.nml"), "").unwrap();
        std::fs::write(project.join("apps/other/app.nml"), "").unwrap();
        std::fs::set_permissions(
            project.join("apps/site/locked"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let unlock = project.join("apps/site/locked");
        // Does the lock bite? Opening a child of a `0o000` directory is
        // EACCES for everyone but root (the crate's ratchet keeps
        // `read_dir` out of its source; a probe by `open` asks the same).
        if !matches!(
            std::fs::File::open(unlock.join("probe")),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
        ) {
            return; // root: the lock does not bite
        }
        let resolver = PackageResolver::new(None, test_events().0);
        let roots = vec![ws.to_path_buf()];
        let refused = resolver.resolve(&project.join("apps/site/app.nml"), &view(&roots));
        let other = resolver.resolve(&project.join("apps/other/app.nml"), &view(&roots));
        std::fs::set_permissions(&unlock, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(refused.resolution, Resolution::Refused),
            "{:?}",
            refused.notes
        );
        assert!(
            refused.notes.iter().any(|n| {
                n.code == Some(nml_core::diagnostic::codes::UNIVERSE_TRUNCATED)
                    && n.severity == Severity::Error
            }),
            "{:?}",
            refused.notes
        );
        assert!(
            matches!(other.resolution, Resolution::Bound(_)),
            "{:?}",
            other.notes
        );
    }
}

#[cfg(test)]
mod r92_tests {
    use super::*;

    use nml_validate::test_support::{DEMO_CORE as CORE, DEMO_MANIFEST as MANIFEST, publish_demo};
    use nml_validate::workspace::ReadError;

    /// A workspace folder the oracle cannot canonicalize (removed while
    /// the editor was open, no buffer under it) has no universe: nothing
    /// is indexed and nothing is denied — never a panic, never a
    /// phantom denial.
    #[test]
    fn a_folder_that_no_longer_exists_indexes_nothing_and_denies_nothing() {
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let gone = std::env::temp_dir().join(format!("nml-pkg-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&gone);
        let roots = vec![gone.clone()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &[],
            documents: &NoDocs,
        };
        let index = resolver.index(&gone, &view);
        assert!(index.files.is_empty(), "{:?}", index.files);
        assert!(index.denials.is_empty(), "{:?}", index.denials);
    }

    /// A document more than `MAX_COMPONENTS` directories below its root
    /// cannot be keyed: the kernel's sentence (`more than 64 path
    /// components`) is the editor's one ERROR row and the document is
    /// REFUSED — `nml check` fails that target with the same sentence
    /// and judges nothing, so the editor validates nothing either (it
    /// used to warn and validate the document in the open registry
    /// mode) — and nothing under it is indexed (the walk's exact skip
    /// at the bound): never a panic, never a binding.
    #[test]
    fn a_document_past_the_component_bound_is_refused_with_the_kernels_sentence() {
        let ws = crate::scratch::Scratch::new("pkg-test-deep");
        std::fs::write(ws.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        let deep = (0..nml_validate::workspace::MAX_COMPONENTS)
            .fold(ws.to_path_buf(), |p, i| p.join(format!("d{i}")));
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("x.flow.nml");
        std::fs::write(&file, "").unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let roots = vec![ws.to_path_buf()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &[],
            documents: &NoDocs,
        };
        let resolved = resolver.resolve(&file, &view);
        assert!(
            matches!(resolved.resolution, Resolution::Refused),
            "{:?}",
            resolved.notes
        );
        assert_eq!(resolved.notes.len(), 1, "{:?}", resolved.notes);
        let note = &resolved.notes[0];
        assert_eq!(note.severity, Severity::Error, "{note:?}");
        assert!(note.code.is_none(), "{note:?}");
        assert_eq!(
            note.message,
            "more than 64 path components — nothing this deep is keyable; flatten the tree, or \
             move the file where the walk lists it"
        );
        let index = resolver.index(&ws, &view);
        assert!(
            index.files.iter().all(|f| !f.starts_with(&deep)),
            "{:?}",
            index.files
        );
        // The directory AT the bound is a reported skip (round 92's
        // fail-closed row) — said by the index, once.
        assert_eq!(index.denials.len(), 1, "{:?}", index.denials);
        assert!(
            index.denials[0].contains("[NML2090]")
                && index.denials[0].contains("64-component bound"),
            "{:?}",
            index.denials
        );
    }

    /// A directory the walk could not ENTER holds content the index never
    /// saw: the kernel's fail-closed skip rows (NML2090 — at the
    /// component bound; a name no key can carry) are denials of the
    /// index, said in the kernel's own sentence — and so is a `.nml`
    /// FILE so named (unjudged, unindexed: the gate fails on it too,
    /// RFC 0026 B-2). A by-policy skip (a dot-directory) stays silent,
    /// as it always was.
    #[cfg(unix)]
    #[test]
    fn a_directory_the_walk_cannot_enter_is_a_denial_of_the_index() {
        let ws = crate::scratch::Scratch::new("pkg-test-unenterable");
        std::fs::write(ws.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(ws.join("core.model.nml"), CORE).unwrap();
        std::fs::create_dir_all(ws.join("tenants/ev\\il")).unwrap();
        std::fs::write(ws.join("tenants/ev\\il/hidden.flow.nml"), "").unwrap();
        std::fs::create_dir_all(ws.join("tenants/.hidden")).unwrap();
        std::fs::write(ws.join("tenants/.hidden/x.flow.nml"), "").unwrap();
        std::fs::create_dir_all(ws.join("tenants/cu")).unwrap();
        std::fs::write(ws.join("tenants/cu/ev\\il.flow.nml"), "").unwrap();
        // A chain past the component bound: `tenants/d0/…/d62` is the
        // depth-64 key, the last keyable one; `d63` beneath it is never
        // listed — a `componentBound` row at the depth-64 key, and a denial.
        let deep: String = (0..nml_validate::workspace::MAX_COMPONENTS)
            .map(|i| format!("d{i}/"))
            .collect();
        std::fs::create_dir_all(ws.join(format!("tenants/{deep}"))).unwrap();
        std::fs::write(ws.join(format!("tenants/{deep}below.flow.nml")), "").unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let roots = vec![ws.to_path_buf()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &[],
            documents: &NoDocs,
        };
        let index = resolver.index(&ws, &view);
        assert_eq!(index.denials.len(), 3, "{:?}", index.denials);
        let denial = |what: &str| {
            index
                .denials
                .iter()
                .find(|d| d.contains(what))
                .unwrap_or_else(|| panic!("{what}: {:?}", index.denials))
                .clone()
        };
        assert!(
            denial("a directory the walk never entered").starts_with(
                "[NML2090] the walk skipped an entry under `tenants` whose name no key can carry (`ev\\il`"
            ),
            "{:?}",
            index.denials
        );
        assert!(
            denial("a `.nml` file no verb judged").starts_with(
                "[NML2090] the walk skipped an entry under `tenants/cu` whose name no key can carry (`ev\\il.flow.nml`"
            ),
            "{:?}",
            index.denials
        );
        assert!(
            denial("64-component bound").starts_with("[NML2090] the walk skipped `tenants/d0/")
                && denial("64-component bound")
                    .contains("/d62`: a directory at the 64-component bound the walk never enters"),
            "{:?}",
            index.denials
        );
        assert!(
            index
                .files
                .iter()
                .all(|f| !f.to_string_lossy().contains("hidden")
                    && !f.to_string_lossy().contains("below")),
            "{:?}",
            index.files
        );
    }

    struct NoDocs;

    impl OpenDocuments for NoDocs {
        fn text(&self, _: &Path) -> Option<String> {
            None
        }

        fn stamp(&self, _: &Path) -> Option<u64> {
            None
        }
    }

    /// One unsaved buffer, its text and stamp fixed.
    struct OneBuffer {
        path: PathBuf,
        text: String,
    }

    impl OpenDocuments for OneBuffer {
        fn text(&self, path: &Path) -> Option<String> {
            (path == self.path).then(|| self.text.clone())
        }

        fn stamp(&self, path: &Path) -> Option<u64> {
            (path == self.path).then_some(1)
        }
    }

    /// A store that also answers one fixed stamp for every DIRECTORY, so
    /// the derived-root memo's ancestor-chain fingerprints cannot move
    /// under a test. They do otherwise: the scratch root's parent is the
    /// shared temp directory, whose mtime every concurrent test's
    /// `Scratch::new` bumps — measured, that alone re-derived, and the
    /// buffer-set key could be deleted with the whole suite green in a
    /// parallel run (RED only under `--test-threads=1`). With the chain
    /// held still, the only input that moves between two questions is
    /// the one the test changes.
    struct FixedDirectories<D>(D);

    impl<D: OpenDocuments> OpenDocuments for FixedDirectories<D> {
        fn text(&self, path: &Path) -> Option<String> {
            self.0.text(path)
        }

        fn stamp(&self, path: &Path) -> Option<u64> {
            if path.is_dir() {
                return Some(0);
            }
            self.0.stamp(path)
        }
    }

    /// The DERIVED root's memo, all three of its freshness inputs. A
    /// document outside every workspace folder gets the root the kernel
    /// derives for it, memoized per document DIRECTORY and held while an
    /// open buffer sits under it. Nothing tested what makes that memo go
    /// stale: measured, the ancestor-fingerprint check, the buffer-set
    /// check, the eviction and the superseded universe's removal could
    /// EACH be deleted with the whole workspace suite green.
    ///
    /// Here the document starts with no `.git` and no marker above it,
    /// so the kernel fences at its own directory (E21's no-VCS regime);
    /// then a `.git` DIRECTORY and a manifest appear one level up, and
    /// the fence — and with it the root — must move. An editor sees
    /// exactly this the first time somebody runs `git init` in a folder
    /// they are already editing.
    #[test]
    fn a_derived_root_is_re_derived_when_an_ancestor_gains_a_fence() {
        let ws = crate::scratch::Scratch::new("pkg-test-derived-fence");
        let inner = ws.join("outer/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let doc = inner.join("a.nml");
        std::fs::write(&doc, "").unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        // NO workspace folder: the document takes R1's third rung. The
        // buffer is what keeps the memo alive — without one the memo is
        // swept on every call and nothing could go stale.
        let roots: Vec<PathBuf> = Vec::new();
        let buffers = vec![doc.clone()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &NoDocs,
        };
        let first = resolver
            .resolve(&doc, &view)
            .root
            .expect("a document outside every folder still gets a derived root");
        assert_eq!(
            first.0, inner,
            "with no fence above it the universe is the document's own directory"
        );
        // A repository and a manifest appear one level up.
        std::fs::create_dir_all(ws.join("outer/.git")).unwrap();
        std::fs::write(ws.join("outer/demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(ws.join("outer/core.model.nml"), CORE).unwrap();
        let second = resolver.resolve(&doc, &view).root.expect("still derivable");
        assert_eq!(
            second.0,
            ws.join("outer"),
            "the fence moved, so the derived root must move with it — the memo is keyed on \
             the ancestor chain's fingerprints for exactly this"
        );
        assert!(
            resolver.holds_universe_at(&second.0),
            "the new root's universe is the one that answers now"
        );
        assert!(
            !resolver.holds_universe_at(&first.0),
            "the superseded derivation took its universe with it: a universe keyed on a root \
             nothing derives any more is held for as long as a buffer sits under it"
        );
    }

    /// The same memo's BUFFER-SET input, and its eviction. An unsaved
    /// manifest one level up is a root MARKER the walk sees through the
    /// overlay, so opening it moves the derived root without a byte
    /// reaching the disk — and closing every buffer under a directory
    /// drops its entry, so the next open re-derives rather than
    /// answering from a memo nothing is keeping fresh. Every directory's
    /// fingerprint is held still ([`FixedDirectories`]) so the buffer
    /// set is the ONLY input that moves: deleting the buffer-set key
    /// must make the second question answer from the memo.
    #[test]
    fn a_derived_root_follows_the_buffer_set_and_is_dropped_with_it() {
        let ws = crate::scratch::Scratch::new("pkg-test-derived-buffers");
        let inner = ws.join("outer/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let doc = inner.join("a.nml");
        std::fs::write(&doc, "").unwrap();
        std::fs::create_dir_all(ws.join("outer/.git")).unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let roots: Vec<PathBuf> = Vec::new();
        let only_doc = vec![doc.clone()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &only_doc,
            documents: &FixedDirectories(NoDocs),
        };
        assert_eq!(
            resolver.resolve(&doc, &view).root.expect("derivable").0,
            inner,
            "inside the fence, with no marker, the root is the document's own directory"
        );
        // An UNSAVED manifest beside the fence: a marker the disk lacks.
        let manifest = ws.join("outer/demo.package.nml");
        let with_manifest = vec![doc.clone(), manifest.clone()];
        let docs = FixedDirectories(OneBuffer {
            path: manifest.clone(),
            text: MANIFEST.to_string(),
        });
        let buffered = WorkspaceView {
            roots: &roots,
            buffers: &with_manifest,
            documents: &docs,
        };
        assert_eq!(
            resolver.resolve(&doc, &buffered).root.expect("derivable").0,
            ws.join("outer"),
            "an unsaved marker moves the derived root: the memo is keyed on the buffer set too"
        );
        // Every buffer under the document's directory closes: the entry
        // is dropped, so the next question is asked of the kernel again
        // — and the answer is the one the CURRENT tree gives.
        let elsewhere = vec![ws.join("somewhere-else.nml")];
        let closed = WorkspaceView {
            roots: &roots,
            buffers: &elsewhere,
            documents: &FixedDirectories(NoDocs),
        };
        assert_eq!(
            resolver.resolve(&doc, &closed).root.expect("derivable").0,
            inner,
            "with the manifest buffer gone the root is the fenced directory again"
        );
        // …and the entry itself is not kept for ever: the memo holds
        // only directories an open buffer still sits under, so asking
        // about ANOTHER document (with only ITS buffer open) sweeps the
        // first one rather than accumulating both.
        let other_dir = ws.join("second");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other = other_dir.join("b.nml");
        std::fs::write(&other, "").unwrap();
        let only_other = vec![other.clone()];
        let other_view = WorkspaceView {
            roots: &roots,
            buffers: &only_other,
            documents: &FixedDirectories(NoDocs),
        };
        let _ = resolver.resolve(&other, &other_view);
        assert_eq!(
            resolver.held_derived(),
            1,
            "the derivation of a directory no buffer sits under any more was swept, not kept"
        );
        assert!(
            !resolver.holds_universe_at(&inner),
            "and its universe went with it: a DERIVED universe lives only while a buffer sits \
             under its root (a workspace FOLDER's is kept whatever the buffers are)"
        );
    }

    /// A re-derivation that names the SAME root keeps its universe. An
    /// ancestor's fingerprint moves for many reasons that change no
    /// verdict — a file saved beside the fence, a temp entry above it —
    /// and each one re-asks the kernel, rightly (a marker there would
    /// move the root). Only a root that CHANGED takes its universe with
    /// it (the fence test above); measured, the memo dropped the universe
    /// on every re-derivation, a full re-walk for a sibling's save.
    #[test]
    fn a_re_derivation_naming_the_same_root_keeps_its_universe() {
        let ws = crate::scratch::Scratch::new("pkg-test-derived-same-root");
        let inner = ws.join("outer/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let doc = inner.join("a.nml");
        std::fs::write(&doc, "").unwrap();
        std::fs::create_dir_all(ws.join("outer/.git")).unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let roots: Vec<PathBuf> = Vec::new();
        let buffers = vec![doc.clone()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &NoDocs,
        };
        assert_eq!(
            resolver.resolve(&doc, &view).root.expect("derivable").0,
            inner
        );
        let first = resolver
            .universe_at(&inner)
            .expect("the derived root's universe is cached");
        // An entry appears ABOVE the fence: that directory's fingerprint
        // moves (asserted, so the re-derivation below is not vacuous), the
        // kernel is asked again, and it names the same root.
        let above = ws.to_path_buf();
        let was = std::fs::metadata(&above).unwrap().modified().unwrap();
        std::fs::write(above.join("unrelated.txt"), "").unwrap();
        assert_ne!(
            std::fs::metadata(&above).unwrap().modified().unwrap(),
            was,
            "the ancestor's mtime moved"
        );
        assert_eq!(
            resolver
                .resolve(&doc, &view)
                .root
                .expect("still derivable")
                .0,
            inner
        );
        let second = resolver
            .universe_at(&inner)
            .expect("the same root's universe is still cached");
        assert!(
            Arc::ptr_eq(&first, &second),
            "the same root keeps its universe: a re-derivation is not a re-discovery"
        );
    }

    /// NESTED workspace folders — VS Code allows them, and a
    /// multi-root workspace that lists a repository and one of its
    /// subdirectories is the ordinary way to get one. The anchor is the
    /// FIRST folder in the list that contains the document, so the
    /// universe a nested document is judged in depends on the ORDER the
    /// operator added the folders: listed outer-first the manifest above
    /// governs, inner-first it is outside the universe and invisible.
    /// Nothing pinned that, so either reading could have been swapped in
    /// silently. Pinned here as the rule the code states; whether the
    /// DEEPEST containing folder should win instead is an owner decision.
    #[test]
    fn a_document_under_nested_workspace_folders_anchors_at_the_first_listed() {
        let ws = crate::scratch::Scratch::new("pkg-test-nested-folders");
        let outer = ws.join("outer");
        let inner = outer.join("apps");
        std::fs::create_dir_all(inner.join("one")).unwrap();
        std::fs::write(outer.join("demo.package.nml"), MANIFEST).unwrap();
        std::fs::write(outer.join("core.model.nml"), CORE).unwrap();
        // The manifest's glob is `apps/*/app.nml`, relative to the root:
        // which root the document is keyed against is the whole question.
        let doc = inner.join("one/app.nml");
        std::fs::write(&doc, "").unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let outer_first = vec![outer.clone(), inner.clone()];
        let outer_view = WorkspaceView {
            roots: &outer_first,
            buffers: &[],
            documents: &NoDocs,
        };
        let bound = resolver.resolve(&doc, &outer_view);
        assert_eq!(
            bound.root.as_ref().map(|(p, _)| p.as_path()),
            Some(outer.as_path()),
            "the FIRST folder containing the document is its universe"
        );
        assert!(
            matches!(bound.resolution, Resolution::Bound(_)),
            "and the manifest at that folder governs it"
        );
        // The same two folders in the other order: the inner one is the
        // universe, the manifest sits ABOVE it, and nothing governs.
        let inner_first = vec![inner.clone(), outer.clone()];
        let inner_view = WorkspaceView {
            roots: &inner_first,
            buffers: &[],
            documents: &NoDocs,
        };
        let unbound = resolver.resolve(&doc, &inner_view);
        assert_eq!(
            unbound.root.as_ref().map(|(p, _)| p.as_path()),
            Some(inner.as_path())
        );
        assert!(
            matches!(unbound.resolution, Resolution::Unbound),
            "keyed against the nested folder the glob no longer matches, and the manifest \
             above it is outside the universe"
        );
    }

    /// The buffer set is part of a universe's freshness: a manifest
    /// buffer opened AFTER the universe was cached (an unsaved
    /// `demo.package.nml` the disk lacks) rediscovers it — the file that
    /// was unbound in the open universe binds under the buffered
    /// manifest on the next resolve. (A cache that compared only its
    /// reads served the stale, manifest-less universe: nothing had been
    /// read, so nothing had changed.)
    #[test]
    fn a_buffer_opened_after_the_universe_was_cached_rediscovers_it() {
        let ws = crate::scratch::Scratch::new("pkg-test-late-buffer");
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        std::fs::write(project.join("core.model.nml"), CORE).unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(None, events);
        let roots = vec![ws.to_path_buf()];
        let none = WorkspaceView {
            roots: &roots,
            buffers: &[],
            documents: &NoDocs,
        };
        assert!(
            matches!(
                resolver
                    .resolve(&project.join("demo.nml"), &none)
                    .resolution,
                Resolution::Unbound
            ),
            "no manifest on disk: unbound"
        );
        let g1 = resolver.generation();
        let manifest_path = project.join("demo.package.nml");
        let buffers = vec![manifest_path.clone()];
        let docs = OneBuffer {
            path: manifest_path.clone(),
            text: MANIFEST.to_string(),
        };
        let with_buffer = WorkspaceView {
            roots: &roots,
            buffers: &buffers,
            documents: &docs,
        };
        match resolver
            .resolve(&project.join("demo.nml"), &with_buffer)
            .resolution
        {
            Resolution::Bound(b) => {
                assert_eq!(b.class, ClaimClass::Workspace);
                assert_eq!(b.manifest.as_deref(), Some(manifest_path.as_path()));
            }
            Resolution::Unbound | Resolution::Refused => panic!("the late buffer must bind"),
        }
        assert!(resolver.generation() > g1, "rediscovered");
    }

    /// The store's pointers are part of a universe's freshness: a
    /// package re-published (its `current` pointer moved) AFTER the
    /// universe was cached rediscovers it — the next resolve binds to
    /// the new version. (A cache that compared only its reads and
    /// buffers served the old store package forever.)
    #[test]
    fn a_store_pointer_moved_after_the_universe_was_cached_rediscovers_it() {
        let ws = crate::scratch::Scratch::new("pkg-test-late-pointer");
        let store_base = crate::scratch::Scratch::new("pkg-test-late-pointer-store");
        let store = Store::at(store_base.to_path_buf());
        publish_demo(&store);
        let project = ws.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("demo.nml"), "").unwrap();
        let (events, _rx) = tokio::sync::mpsc::channel(8);
        let resolver = PackageResolver::new(Some(Store::at(store_base.to_path_buf())), events);
        let roots = vec![ws.to_path_buf()];
        let view = WorkspaceView {
            roots: &roots,
            buffers: &[],
            documents: &NoDocs,
        };
        let version = |r: Resolved| match r.resolution {
            Resolution::Bound(b) => b.package_version,
            Resolution::Unbound | Resolution::Refused => {
                panic!("the store package binds: {:?}", r.notes)
            }
        };
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &view)),
            "0.1.0"
        );
        let g1 = resolver.generation();
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &view)),
            "0.1.0"
        );
        assert_eq!(
            resolver.generation(),
            g1,
            "nothing moved: the cache answers"
        );
        let newer = SchemaPackage::from_parts(
            &MANIFEST.replace("version = \"0.1.0\"", "version = \"0.2.0\""),
            |_| Ok(CORE.to_string()),
        )
        .expect("the newer package loads");
        store.publish(&newer).expect("publish 0.2.0");
        assert_eq!(
            version(resolver.resolve(&project.join("demo.nml"), &view)),
            "0.2.0",
            "the moved pointer rediscovers"
        );
        assert!(resolver.generation() > g1);
    }

    /// Source-level ratchet: EVERY disk read this crate makes goes
    /// through the kernel's one reader (`read_input` under a root,
    /// `read_leaf` at a file's own parent — both `read_beneath`, the
    /// race-free chain). A by-path open (`File::open`, `fs::read`,
    /// `read_to_string`, or the kernel's own `open_beneath` called
    /// directly here) compiles clean, passes every test
    /// that does not race, and reads through a directory swapped for a
    /// link after the walk classified it — the divergence the one reader
    /// closed. Product code only: the `#[cfg(test)] mod` blocks are cut
    /// out first (tests read fixtures however they like).
    #[test]
    fn every_disk_read_goes_through_the_kernels_reader() {
        use nml_validate::test_support::scan::{blank_comments_and_strings, cfg_test_ranges};
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let forbidden = [
            "File::open(",
            "fs::read(",
            "read_to_string(",
            "open_beneath(",
        ];
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for (name, _) in crate::wasi_fs::read_dir(&dir).expect("crate src readable") {
                let path = dir.join(name);
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source readable");
                let mut clean = blank_comments_and_strings(&text);
                for (start, end) in cfg_test_ranges(&clean.clone()) {
                    clean.replace_range(start..end, &" ".repeat(end - start));
                }
                let collapsed: String = clean.split_whitespace().collect::<Vec<_>>().join("");
                for needle in forbidden {
                    if collapsed.contains(needle) {
                        offenders.push(format!("{}: {needle}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a by-path disk read outside the kernel's reader — use `read_input` (under a \
             root) or `read_leaf` (at the file's own parent):\n{}",
            offenders.join("\n")
        );
        // And the walk's disk case IS the rooted reader: exactly one
        // `read_input(` in this file, inside `discover_root` — a closure
        // that reached for the leaf read instead (it anchors at the
        // parent and follows a swapped one) would pass everything above.
        // The needles are spelled by `concat!` so this test is never its
        // own hit.
        let own = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/packages.rs"),
        )
        .expect("own source");
        let mut product = blank_comments_and_strings(&own);
        for (start, end) in cfg_test_ranges(&product.clone()) {
            product.replace_range(start..end, &" ".repeat(end - start));
        }
        let rooted = concat!("read_", "input(");
        let hits: Vec<usize> = product.match_indices(rooted).map(|(i, _)| i).collect();
        assert_eq!(hits.len(), 1, "rooted reads in packages.rs: {}", hits.len());
        let before = &product[..hits[0]];
        let fn_start = before.rfind("\n    fn ").expect("inside a method");
        assert!(
            before[fn_start..].starts_with("\n    fn discover_root("),
            "the rooted read is not the walk's disk case"
        );
        let leaf = concat!("read_", "leaf(");
        let hits: Vec<usize> = product.match_indices(leaf).map(|(i, _)| i).collect();
        assert_eq!(hits.len(), 1, "leaf reads in packages.rs: {}", hits.len());
        let before = &product[..hits[0]];
        let fn_start = before
            .rfind("\npub(crate) fn ")
            .expect("inside a crate-visible fn");
        assert!(
            before[fn_start..].starts_with(concat!("\npub(crate) fn read_", "input_at_leaf(")),
            "the leaf read is not the kind adapter's"
        );
    }

    /// The editor's leaf read (the kernel's) REFUSES a file that is not
    /// UTF-8 (`not UTF-8`, the CLI's word for the same file) — it never
    /// decodes it lossily into a document the index would then judge as
    /// text the file does not hold.
    #[test]
    fn the_indexed_read_refuses_a_file_that_is_not_utf8() {
        let ws = crate::scratch::Scratch::new("pkg-test-not-utf8");
        let bad = ws.join("bad.nml");
        std::fs::write(&bad, b"thing t:\n    v = \"\xff\xfe\"\n").unwrap();
        let err = read_leaf(&bad, 1024, "an indexed workspace file").expect_err("refused");
        assert!(matches!(err, ReadError::NotUtf8), "{err}");
        assert_eq!(err.to_string(), "not UTF-8");
        let good = ws.join("good.nml");
        std::fs::write(&good, "thing t:\n").unwrap();
        assert_eq!(
            read_leaf(&good, 1024, "an indexed workspace file").unwrap(),
            "thing t:\n"
        );
    }
}
