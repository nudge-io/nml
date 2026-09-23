//! Manifest claims and the governing binding (RFC 0019 item 0, step 0c;
//! Layer A — no I/O). A [`Universe`] is the LIVE claims and configs of one
//! root; [`governing`] answers "which binding governs this key" with the
//! package loader's selection semantics — pins first, then unambiguous
//! auto-association, first match in declaration order — under the three
//! hardening rules of *Resolution inputs must not be author-writable*:
//!
//! - **R5′** a workspace manifest governs only keys under its own
//!   directory (an anchor above `dir(M)` changes only the glob's relative
//!   base);
//! - **rule 3** two live workspace manifests declaring one name are BOTH
//!   candidates — an ambiguous claim (denied), never a nearest shadow;
//! - **R3′/R4** (applied by discovery, consumed here) inert inputs are
//!   absent from the universe, and anchors are manifest-derived.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use nml_core::project::ProjectConfig;

use crate::package::{PackageError, SchemaPackage, ValidatorBinding, valid_package_name};
use crate::schema::SchemaValidator;

use super::paths::{SourceKey, WorkspaceRoot};
use crate::fs::FsError;

/// Where a claim's package definition came from — resolution precedence,
/// descending (RFC 0035's delivery channels): a committed workspace
/// manifest beats an embedder's in-binary package beats the per-user
/// store beats the builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClaimClass {
    Workspace,
    Injected,
    Store,
    Builtin,
}

impl ClaimClass {
    /// The human-facing source label — the one vocabulary the CLI's
    /// `binding` line and the editor's `nml/schemaInfo` `source` share.
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace manifest",
            Self::Injected => "in-binary",
            Self::Store => "store current",
            Self::Builtin => "builtin",
        }
    }

    /// The stable machine tag (`nml binding --json`'s `class`).
    pub fn tag(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Injected => "injected",
            Self::Store => "store",
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

impl BindingStep {
    /// The human spelling (`nml binding`'s `step` line).
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::AutoAssociated => "auto-associated",
        }
    }

    /// The `--json` vocabulary's value (`binding.step`), lowerCamel like
    /// every value in the stream.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::AutoAssociated => "autoAssociated",
        }
    }
}

/// The three claim classes that come from OUTSIDE the workspace walk —
/// anchored at the root's live markers, never at a manifest key — the
/// only classes [`ExternalClaim::new`] accepts. The kernel's own
/// vocabulary (it names each class's remedy in a denial), spoken here
/// unchanged.
pub use nml_core::layers::ExternalClass;

/// A package from OUTSIDE the workspace walk — injected, store or
/// builtin — as a caller hands it to [`discover`](fn@super::discover): the
/// walk anchors it at its live, non-nested marker directories and mints
/// the [`ManifestClaim`] itself. The class is taken by type
/// ([`ExternalClass`]), so no caller can hand the walk a workspace claim
/// — every claim in a [`Discovery`](super::Discovery) was minted by the
/// walk that produced it, whatever its origin.
#[derive(Debug, Clone)]
pub struct ExternalClaim {
    pub(crate) package: Arc<SchemaPackage>,
    pub(crate) class: ExternalClass,
}

impl ExternalClaim {
    pub fn new(package: Arc<SchemaPackage>, class: ExternalClass) -> Self {
        Self { package, class }
    }

    pub fn name(&self) -> &str {
        &self.package.manifest.name
    }

    pub fn package(&self) -> &Arc<SchemaPackage> {
        &self.package
    }

    pub fn class(&self) -> ExternalClass {
        self.class
    }
}

impl From<ExternalClass> for ClaimClass {
    fn from(class: ExternalClass) -> Self {
        match class {
            ExternalClass::Injected => Self::Injected,
            ExternalClass::Store => Self::Store,
            ExternalClass::Builtin => Self::Builtin,
        }
    }
}

/// Where a claim came from — the ONE fact its class, its manifest key
/// and its anchor were three spellings of, and had to agree on. A live
/// WORKSPACE manifest the walk settled carries its key and its R4
/// anchor (the nearest live project config or own-marker directory at
/// or ABOVE `dir(M)`, else `dir(M)` — manifest-derived, never
/// content-derived); a package from OUTSIDE the walk (injected, store,
/// builtin) carries its class and its live, non-nested marker
/// directories (a marker under a live marker of the same package is
/// inert; the unique one containing a key anchors it, else the root).
/// Both origins are minted by the walk alone (`ManifestClaim` has no
/// public constructor: a workspace claim is settled from a live
/// manifest, an external one from the [`ExternalClaim`] a caller handed
/// in, whose class is a type), so a workspace claim without its key —
/// or a claim the walk never saw — is UNREPRESENTABLE: no `expect`, no
/// fail-closed arm anywhere downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOrigin {
    Workspace {
        /// The manifest file's key.
        manifest: SourceKey,
        anchor: SourceKey,
        /// Settled by the walk: no strictly shallower live claim reaches
        /// an ancestor of the manifest's directory — under no unit root
        /// and in no gap of a shallower glob. Only such a claim mints
        /// budget units (a tenant's live manifest in root-unit content
        /// mints none), so only its unit layout is worth a word
        /// (NML2092).
        operator_level: bool,
    },
    External {
        class: ExternalClass,
        markers: Vec<SourceKey>,
    },
}

impl ClaimOrigin {
    /// The resolution-precedence class ([`ClaimClass`] orders them).
    pub fn class(&self) -> ClaimClass {
        match self {
            Self::Workspace { .. } => ClaimClass::Workspace,
            Self::External { class, .. } => (*class).into(),
        }
    }

    /// The manifest file's key — a workspace claim's only.
    pub fn manifest(&self) -> Option<&SourceKey> {
        match self {
            Self::Workspace { manifest, .. } => Some(manifest),
            Self::External { .. } => None,
        }
    }
}

/// Entries the validator table holds before the least recently used one
/// goes — the bound the editor's validator cache carried; a run or an
/// editor session rarely touches more than a handful of bindings.
/// UNPUBLISHED: internal: the validator table's in-memory size, invisible to any result
const VALIDATOR_MEMO_CAP: usize = 64;

/// A bounded memo, least recently used out: a known key answers the held
/// value (and counts as used); an unknown one is built, held and
/// answered, the entry longest unused going once `cap` are held. (A
/// table that forgot EVERYTHING at the cap rebuilt every validator a
/// manifest with more than `cap` bindings touched — one build per
/// resolve past the cap, per pull in the editor.)
struct Lru<V> {
    entries: HashMap<MemoKey, (V, u64)>,
    tick: u64,
    cap: usize,
}

/// A table key: the package's content hash and the binding's name.
type MemoKey = (String, String);

impl<V: Clone> Lru<V> {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            cap,
        }
    }

    fn get_or_insert_with(&mut self, key: MemoKey, build: impl FnOnce() -> V) -> V {
        self.tick += 1;
        let now = self.tick;
        if let Some((held, used)) = self.entries.get_mut(&key) {
            *used = now;
            return held.clone();
        }
        let value = build();
        if self.entries.len() >= self.cap {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (value.clone(), now));
        value
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The validators a universe's bindings build, memoized per (package
/// content hash, binding name) — ONE build per binding per universe,
/// whichever front end asks and however many files the binding governs
/// (`nml check tenants/` built the validator once PER TARGET; the editor
/// kept a cache of its own keyed the same way). The hash covers the
/// manifest AND every declared source, so a fixed source is a new key
/// and a memoized failure can never outlive its cause. Held by the
/// [`Discovery`](super::discover::Discovery) and consulted by
/// `resolve_file`, so "this binding cannot build its validator" is a
/// KERNEL finding (NML2091) every front end reports identically, never a
/// judgement each front end makes for itself. The failure is memoized
/// like the success. Strictness is the BINDING's own
/// (`ValidatorBinding::strict`, applied by `SchemaPackage::validator`):
/// no front end applies a strictness of its own on top — a bound file's
/// verdict is the binding's in every front end.
pub struct ValidatorMemo {
    built: Mutex<Lru<Built>>,
}

/// One table entry: the built validator, or the package layer's error
/// (memoized alike — the failure costs what the success costs).
pub type Built = Result<Arc<SchemaValidator>, Arc<PackageError>>;

impl Default for ValidatorMemo {
    fn default() -> Self {
        Self {
            built: Mutex::new(Lru::new(VALIDATOR_MEMO_CAP)),
        }
    }
}

impl std::fmt::Debug for ValidatorMemo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.built.lock().unwrap_or_else(|e| e.into_inner()).len();
        write!(f, "ValidatorMemo({n} built)")
    }
}

impl ValidatorMemo {
    /// The validator of `binding` in `claim`'s package — built once per
    /// (content hash, binding) and shared from then on. The error is the
    /// package layer's own ([`PackageError::Sources`]: the first failing
    /// source's findings).
    pub fn build(&self, claim: &ManifestClaim, binding: &ValidatorBinding) -> Built {
        let key = (claim.content_hash.clone(), binding.name.clone());
        self.built
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(key, || {
                claim
                    .package
                    .validator(binding)
                    .map(Arc::new)
                    .map_err(Arc::new)
            })
    }

    /// How many validators (and failures) the table holds — the pin's
    /// window into "built once".
    pub fn len(&self) -> usize {
        self.built.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One live package definition and where it came from — minted by the
/// walk alone: no public constructor, and every field but the package
/// crate-private, so a claim cannot be assembled outside the kernel.
#[derive(Debug, Clone)]
pub struct ManifestClaim {
    pub package: Arc<SchemaPackage>,
    pub(crate) origin: ClaimOrigin,
    pub(crate) content_hash: String,
    /// How the denial family names this claim's manifest: the manifest
    /// KEY for a workspace claim (never an absolute path), the class
    /// label otherwise.
    pub(crate) manifest_label: String,
}

impl ManifestClaim {
    fn new(package: Arc<SchemaPackage>, origin: ClaimOrigin) -> Self {
        let content_hash = package.content_hash();
        let manifest_label = match &origin {
            ClaimOrigin::Workspace { manifest, .. } => manifest.as_str().to_string(),
            ClaimOrigin::External { class, .. } => {
                format!("<{}>", ClaimClass::from(*class).label())
            }
        };
        Self {
            package,
            origin,
            content_hash,
            manifest_label,
        }
    }

    /// A live workspace manifest's claim — the walk's alone, at the key
    /// it was settled under and the R4 anchor its globs are relative to.
    pub(crate) fn workspace(
        package: Arc<SchemaPackage>,
        manifest: SourceKey,
        anchor: SourceKey,
        operator_level: bool,
    ) -> Self {
        Self::new(
            package,
            ClaimOrigin::Workspace {
                manifest,
                anchor,
                operator_level,
            },
        )
    }

    /// A store/injected/builtin claim — the walk's alone, from the
    /// [`ExternalClaim`] a caller handed in, at the live, non-nested
    /// marker directories the walk settled for it.
    pub(crate) fn external(claim: ExternalClaim, markers: Vec<SourceKey>) -> Self {
        Self::new(
            claim.package,
            ClaimOrigin::External {
                class: claim.class,
                markers,
            },
        )
    }

    /// Where the claim came from.
    pub fn origin(&self) -> &ClaimOrigin {
        &self.origin
    }

    /// The package's content hash.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// How the denial family names this claim's manifest: the manifest
    /// KEY for a workspace claim, the class label otherwise.
    pub fn manifest_label(&self) -> &str {
        &self.manifest_label
    }

    /// The resolution-precedence class.
    pub fn class(&self) -> ClaimClass {
        self.origin.class()
    }

    /// The manifest file's key — a workspace claim's only.
    pub fn manifest(&self) -> Option<&SourceKey> {
        self.origin.manifest()
    }

    pub fn name(&self) -> &str {
        &self.package.manifest.name
    }

    pub fn identity(&self) -> ClaimIdentity<'_> {
        ClaimIdentity {
            package: self.name(),
            content_hash: &self.content_hash,
            class: self.class(),
        }
    }

    /// The directory this claim's globs are relative to for `key`, when
    /// the claim can govern `key` at all (R5′ for workspace manifests).
    fn anchor_for(&self, key: &SourceKey) -> Option<SourceKey> {
        match &self.origin {
            ClaimOrigin::Workspace {
                manifest, anchor, ..
            } => (manifest.dir_contains(key) && anchor.contains(key)).then(|| anchor.clone()),
            ClaimOrigin::External { markers, .. } => Some(
                markers
                    .iter()
                    .find(|d| d.contains(key))
                    .cloned()
                    .unwrap_or_else(SourceKey::root),
            ),
        }
    }

    /// Whether this WORKSPACE claim's content reaches into the directory
    /// `dir` (R3′): the first binding glob, under this claim's anchor,
    /// that can match a path below `dir`. The claimant names the binding
    /// and glob NML2080 reports.
    fn reaches<'a>(&'a self, dir: &SourceKey) -> Option<Claimant<'a>> {
        let anchor = self.anchor_for(dir)?;
        let rel = dir.relative_to(&anchor)?;
        self.package.manifest.validators.iter().find_map(|binding| {
            binding
                .files
                .iter()
                .position(|g| crate::glob::glob_reaches_dir(g, rel))
                .map(|glob| Claimant {
                    claim: self,
                    binding,
                    glob,
                    anchor: anchor.clone(),
                })
        })
    }

    /// Whether the directory `dir` is a BUDGET UNIT ROOT of this claim
    /// (A16 amendment): anchor-relative,
    /// `dir` sits exactly at some binding glob's unit boundary
    /// (`glob::unit_prefix_len` — the start of the glob's last run of
    /// wildcard directory segments), and that glob reaches into it.
    /// `tenants/**/*.flow.nml` and `tenants/*/**` both make every
    /// `tenants/<x>` a unit; `orgs/*/tenants/**` makes every
    /// `orgs/<o>/tenants/<t>` one; `**/*.flow.nml` makes every top-level
    /// directory one; a wildcard-free glob makes none.
    ///
    /// WORKSPACE claims only, and deliberately: a unit is a delegation
    /// boundary an operator committed in a manifest, so the builtin (or
    /// a store package installed for the developer) must not silently
    /// re-shape the entry bound of a repository that committed nothing.
    /// The kernel enforces it structurally too — external claims are
    /// added after the walk, so they cannot be consulted during it.
    fn is_budget_unit(&self, dir: &SourceKey) -> bool {
        let ClaimOrigin::Workspace {
            manifest, anchor, ..
        } = &self.origin
        else {
            return false;
        };
        // R5′: a workspace manifest governs its own subtree only, and its
        // globs are relative to its anchor — the same two tests `bind`
        // applies (`anchor_for`).
        if !(manifest.dir_contains(dir) && anchor.contains(dir)) {
            return false;
        }
        let Some(rel) = dir.relative_to(anchor) else {
            return false;
        };
        if rel.is_empty() {
            return false;
        }
        // A DECLARATION replaces inference for this manifest (item 4's
        // spelling, E38): `dir` is a unit root iff a declared pattern
        // matches it whole — the loader refused any declaration that
        // would leave claimed content in the root unit.
        let declared = &self.package.manifest.budget_units;
        if !declared.is_empty() {
            return declared
                .iter()
                .any(|unit| crate::glob::glob_match(unit, rel));
        }
        let depth = rel.split('/').count();
        self.package.manifest.validators.iter().any(|binding| {
            binding.files.iter().any(|glob| {
                crate::glob::unit_prefix_len(glob) == Some(depth - 1)
                    && crate::glob::glob_reaches_dir(glob, rel)
            })
        })
    }

    /// Bind `key` under this claim: the anchor-relative path against the
    /// package's bindings, first match in declaration order.
    fn bind<'a>(&'a self, key: &SourceKey) -> Option<Claimant<'a>> {
        let anchor = self.anchor_for(key)?;
        let rel = key.relative_to(&anchor)?;
        let (binding, glob) = self.package.binding_match(rel)?;
        Some(Claimant {
            claim: self,
            binding,
            glob,
            anchor,
        })
    }
}

/// The single owner of the human-facing claim identity (A10):
/// `<name> blake3:<hash8>, <source>` — the CLI's `nml binding` and the
/// LSP's hover and diagnostic suffixes print one label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimIdentity<'a> {
    pub package: &'a str,
    pub content_hash: &'a str,
    pub class: ClaimClass,
}

impl ClaimIdentity<'_> {
    pub fn render(&self) -> String {
        format!(
            "{} blake3:{}, {}",
            self.package,
            crate::store::hash8(self.content_hash),
            self.class.label()
        )
    }
}

/// A live `nml-project.nml`.
#[derive(Debug, Clone)]
pub struct ProjectConfigClaim {
    pub path: SourceKey,
    pub config: ProjectConfig,
}

/// What discovery could NOT settle about a universe — the two ways it
/// is closed before a single claim is counted. The variants are
/// exclusive by construction: a truncated `Discovery` carries no load
/// error — `denied()` returns the empty default, whatever the
/// interleaved walk had loaded strictly above the stop before it was
/// cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closure {
    /// Every manifest and config was enumerated and every live one
    /// loaded: the universe is closed iff it holds a workspace claim.
    Complete,
    /// Discovery was cut short (A16): the universe is closed and DENIES
    /// every file — never "no claims".
    Truncated,
    /// This many live workspace manifests and project configs were
    /// discovered but failed to load. They contribute no claims, yet
    /// they CLOSE the universe: a malformed operator input must never
    /// reopen a repo into the permissive default. (E27(3) names
    /// manifests; a failing live project config closes too — its pins
    /// and `autoAssociate` are unknown.)
    Unloadable(usize),
}

impl Closure {
    /// The `--json` vocabulary's word (the closing row's and the
    /// `binding` row's `closure`): `complete`, `truncated` (the whole
    /// universe) or `unloadable` (a live input failed to load) — so
    /// `closed` alone never conflates the three.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Truncated => "truncated",
            Self::Unloadable(_) => "unloadable",
        }
    }
}

/// Which bound a budget unit spent (A16 amendment): the three bounds a
/// unit is charged for on its own, mirroring the three
/// whole-universe shapes of `discover::Truncation` — the root unit's
/// exhaustion under any of them is still the whole universe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitBound {
    /// `MAX_ENTRIES` was reached while listing `stop` (a directory) or
    /// settling the input at `stop` (a file key: settlement probes are
    /// charged as entries).
    Entries,
    /// The unit's `MAX_LIVE_INPUT_BYTES` was spent reading the live
    /// input at `stop` (a manifest, a project config or a declared
    /// source — a file key, not a directory).
    LiveInputBytes,
    /// `stop` could not be listed.
    Unreadable(FsError),
}

impl UnitBound {
    /// The `--json` vocabulary's word (a `truncatedUnits[].why`):
    /// `entries`, `liveInputBytes`, `unreadable`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::LiveInputBytes => "liveInputBytes",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

/// A budget UNIT the walk stopped inside (A16 amendment).
///
/// Narrowing the denial to the unit narrows no universe-wide fact, which
/// is what E28's "closed-denied semantics are never auto-narrowed"
/// forbids: every input the walk did not reach governs only keys under
/// its OWN directory (R5′ for workspace claims, [`ClaimOrigin::External`]
/// for external ones, `nearest_config` for project configs, and rule-3
/// ambiguity through `candidates_named` ∘ `bind`), and the unit root is
/// strictly below the directory of the manifest whose glob induced it,
/// so a truncated unit always leaves at least one live workspace claim
/// standing above it — the universe is still CLOSED, and it is only the
/// per-key denial that is subtree-local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitTruncation {
    /// The unit's root directory: every key under it is denied.
    pub unit: SourceKey,
    /// Where the walk stopped: the directory whose listing reached the
    /// entry bound or could not be listed, or the live INPUT whose bytes
    /// spent the unit's budget.
    pub stop: SourceKey,
    /// Which bound.
    pub why: UnitBound,
}

/// One root's LIVE resolution inputs for one pass (R3′ already applied:
/// inert manifests, configs and markers are simply absent).
#[derive(Debug, Clone, Copy)]
pub struct Universe<'a> {
    pub root: &'a WorkspaceRoot,
    pub claims: &'a [ManifestClaim],
    /// The live project configs, read through [`Self::nearest_config`].
    pub(crate) configs: &'a [ProjectConfigClaim],
    /// What discovery could not settle (truncation, unloadable inputs).
    pub closure: Closure,
    /// Budget units the walk could not enumerate (A16 amendment): every
    /// key under one is denied, and no key outside one is affected.
    pub truncated_units: &'a [UnitTruncation],
    /// The universe's validators, built once per binding ([`ValidatorMemo`]).
    pub validators: &'a ValidatorMemo,
}

/// Whether a universe DECIDES what governs the files under it.
///
/// `Closed`: a workspace manifest was discovered (or discovery was
/// truncated), so the universe's own claims are the whole answer and a
/// file no `files` glob matches is governed by nothing — on purpose.
/// `Open`: none was, so nothing within the fence claims anything and
/// composition is permitted.
///
/// The ONE owner of the two words every surface says. They were two
/// string literals in `nml-cli` (`out.rs`, `main.rs`) and a bare
/// `bool` in the editor, which is how the status bar came to give the
/// OPEN remedy — *commit a `<name>.package.nml`* — over a CLOSED
/// universe, where a manifest already exists and the remedy is a
/// `files` glob. One enum, one `label()`, every surface renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniverseState {
    Open,
    Closed,
}

impl UniverseState {
    /// The wire's word, and the human's.
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

impl Universe<'_> {
    /// Closed iff a workspace manifest was discovered (or discovery was
    /// truncated). Store, injected and builtin packages can BIND files but
    /// never close a universe: a developer's own repo stays open with a
    /// store package installed.
    pub fn is_closed(&self) -> bool {
        self.closure != Closure::Complete || self.workspace_claims() > 0
    }

    /// The same fact as [`Self::is_closed`], as the word every surface
    /// prints ([`UniverseState`]).
    pub fn state(&self) -> UniverseState {
        if self.is_closed() {
            UniverseState::Closed
        } else {
            UniverseState::Open
        }
    }

    /// The unloadable live inputs (zero unless [`Closure::Unloadable`]).
    fn unloadable(&self) -> usize {
        match self.closure {
            Closure::Unloadable(n) => n,
            Closure::Complete | Closure::Truncated => 0,
        }
    }

    /// The discovered workspace-manifest count the closed form reports
    /// (live claims plus inputs that failed to load).
    pub fn workspace_claims(&self) -> usize {
        self.unloadable()
            + self
                .claims
                .iter()
                .filter(|c| matches!(c.origin, ClaimOrigin::Workspace { .. }))
                .count()
    }

    /// The nearest live project config at or above `key`'s directory.
    pub fn nearest_config(&self, key: &SourceKey) -> Option<&ProjectConfigClaim> {
        nearest_config(self.configs, key)
    }
}

impl<'a> Universe<'a> {
    /// The truncated budget unit `key` sits under, if any (A16
    /// amendment): the walk never enumerated this subtree, so nothing
    /// under it binds.
    pub fn truncated_unit(&self, key: &SourceKey) -> Option<&'a UnitTruncation> {
        self.truncated_units.iter().find(|t| t.unit.contains(key))
    }
}

/// One binding that claims a key.
#[derive(Debug, Clone)]
pub struct Claimant<'a> {
    pub claim: &'a ManifestClaim,
    pub binding: &'a ValidatorBinding,
    /// Index of the matching `files` glob.
    pub glob: usize,
    /// The directory the glob matched under.
    pub anchor: SourceKey,
}

/// The governing binding of one key.
#[derive(Debug, Clone)]
pub enum Governing<'a> {
    Bound {
        claimant: Claimant<'a>,
        step: BindingStep,
    },
    /// Two or more claimants — denied (fail-closed), naming all.
    Ambiguous(Vec<Claimant<'a>>),
    /// No binding governs the key; the context default applies (open iff
    /// the universe holds no workspace claim).
    Unbound,
}

/// R5: pins from the nearest live config, in list order — each pin's
/// candidates are the live definitions of that name in the highest class
/// present (every workspace definition is a candidate; rule 3); exactly
/// one binds ⇒ `Bound`, more ⇒ `Ambiguous`, none ⇒ the next pin. Then
/// auto-association (when the live config allows) over all live claims
/// with the same one/more/none rule. A truncated universe binds nothing.
pub fn governing<'a>(u: &Universe<'a>, key: &SourceKey) -> Governing<'a> {
    // A16: a truncated universe binds nothing; a truncated UNIT binds
    // nothing under it (the claims that WOULD have been discovered there
    // govern only keys under their own directories, so the walk's gap is
    // exactly the denial). `resolve_file` turns the second into the
    // key's error finding, the way rule-3 ambiguity is.
    if u.closure == Closure::Truncated || u.truncated_unit(key).is_some() {
        return Governing::Unbound;
    }
    let claims = u.claims;
    let (pins, auto_associate) = match u.nearest_config(key) {
        Some(c) => (c.config.pinned_packages(), c.config.auto_associate),
        None => (Vec::new(), true),
    };
    for pin in pins.iter().filter(|p| valid_package_name(p)) {
        let mut bound: Vec<Claimant<'a>> = candidates_named(claims, pin)
            .filter_map(|c| c.bind(key))
            .collect();
        match bound.len() {
            0 => {}
            1 => {
                return Governing::Bound {
                    claimant: bound.remove(0),
                    step: BindingStep::Pinned,
                };
            }
            _ => return Governing::Ambiguous(bound),
        }
    }
    if !auto_associate {
        return Governing::Unbound;
    }
    // Grouped by name ONCE, in name order (the order the sorted,
    // deduplicated name list walked before): a full scan of the claims
    // per distinct name cost N work per name — quadratic in the live
    // manifests a tenant committed, once per checked file.
    let mut by_name: BTreeMap<&str, Vec<&'a ManifestClaim>> = BTreeMap::new();
    for claim in claims {
        by_name.entry(claim.name()).or_default().push(claim);
    }
    let mut bound: Vec<Claimant<'a>> = by_name
        .into_values()
        .flat_map(|group| {
            let best = group.iter().map(|c| c.class()).min();
            group.into_iter().filter(move |c| Some(c.class()) == best)
        })
        .filter_map(|c| c.bind(key))
        .collect();
    match bound.len() {
        0 => Governing::Unbound,
        1 => Governing::Bound {
            claimant: bound.remove(0),
            step: BindingStep::AutoAssociated,
        },
        _ => Governing::Ambiguous(bound),
    }
}

/// The definitions of `name` in the highest class present: a workspace
/// manifest shadows a same-named store copy, but never another workspace
/// manifest (rule 3).
fn candidates_named<'a>(
    claims: &'a [ManifestClaim],
    name: &str,
) -> impl Iterator<Item = &'a ManifestClaim> + use<'a> {
    let best = claims
        .iter()
        .filter(|c| c.name() == name)
        .map(|c| c.class())
        .min();
    let name = name.to_string();
    claims
        .iter()
        .filter(move |c| c.name() == name && Some(c.class()) == best)
}

/// R3′: the live WORKSPACE claim at a STRICTLY SHALLOWER directory whose
/// content reaches the directory of the input at `key` — outermost
/// first — if any. Inputs beside a manifest (its own config, a sibling
/// manifest) are never inerted by it; only inputs in the subtrees below
/// it can be. Pure glob reach: pins select among definitions, they never
/// decide whether content is claimed.
pub(super) fn claiming_outer<'a>(
    live: &'a [ManifestClaim],
    key: &SourceKey,
    probes: &mut usize,
) -> Option<Claimant<'a>> {
    reaching_outer(live, &key.dir(), probes)
}

/// R3′ over a DIRECTORY: the live WORKSPACE claim at a strictly
/// shallower directory whose content reaches `dir` — outermost first —
/// if any. [`claiming_outer`] for an input is this over the input's
/// directory; the walk's operator-level test asks it of every
/// strict ancestor of a live manifest's directory. Every `reaches`
/// probe is counted into `probes`: the walk charges them to the entry
/// budget of the unit being settled, so N sibling manifests whose
/// globs reach nothing cannot make N inputs beneath them cost N²
/// uncharged work.
pub(super) fn reaching_outer<'a>(
    live: &'a [ManifestClaim],
    dir: &SourceKey,
    probes: &mut usize,
) -> Option<Claimant<'a>> {
    // `live` is the walk's own vector: workspace claims in SETTLEMENT
    // order — (key depth, key), outermost first — with the external
    // claims (no manifest) appended only after the walk. So no re-sort
    // is needed, and a claim whose manifest is deeper than `dir` can
    // never sit at a strict ancestor of it: the scan stops at the
    // first such claim, which is what keeps one directory of N sibling
    // manifests from costing N work per input.
    let depth = dir.depth();
    live.iter()
        .take_while(|c| c.manifest().is_some_and(|m| m.depth() <= depth))
        .filter(|c| {
            c.manifest()
                .is_some_and(|m| m.dir_is_strict_ancestor_of(dir))
        })
        .find_map(|c| {
            *probes += 1;
            c.reaches(dir)
        })
}

/// Whether `dir` is a budget unit root of any of the live OPERATOR-LEVEL
/// claims (A16 amendment; `live` is the walk's operator-level list: only
/// a claim whose manifest sits in no content a strictly shallower live
/// claim reaches may name a unit root, so a tenant's own live manifest
/// can never buy its subtree a budget, wherever it sits). The walk asks
/// this once per directory it lists — below a unit root the answer is
/// inherited, so the glob DP runs once per directory rather than once
/// per ancestor. A claim whose manifest directory does not CONTAIN
/// `dir` is filtered out BEFORE the probe is charged — the discipline
/// [`reaching_outer`] has — since `is_budget_unit` answers `false` for
/// it at once: N operator-level manifests in unreached subtrees used to
/// cost every listed directory in every unit N charged probes (a
/// whole-universe denial from 512 committed entries). Every
/// remaining probe is counted into `probes` and charged by the walk
/// like a listed entry.
pub(super) fn is_budget_unit(live: &[ManifestClaim], dir: &SourceKey, probes: &mut usize) -> bool {
    live.iter()
        .filter(|c| c.manifest().is_some_and(|m| m.dir_contains(dir)))
        .any(|c| {
            *probes += 1;
            c.is_budget_unit(dir)
        })
}

/// The nearest live project config at or above `key`'s directory.
fn nearest_config<'a>(
    configs: &'a [ProjectConfigClaim],
    key: &SourceKey,
) -> Option<&'a ProjectConfigClaim> {
    configs
        .iter()
        .filter(|c| c.path.dir().contains(key))
        .max_by_key(|c| c.path.depth())
}

#[cfg(test)]
mod tests {
    use super::Lru;

    /// The validator table's eviction is LEAST RECENTLY USED, one entry
    /// at a time — never a wholesale clear: a manifest with more bindings
    /// than the cap keeps its hot validators across resolves. Cap 2:
    /// `a`, `b`, `a` touched, `c` built ⇒ `b` goes and `a` stays built
    /// (its builder never runs again).
    #[test]
    fn the_validator_table_evicts_the_least_recently_used_entry_only() {
        let mut lru: Lru<u32> = Lru::new(2);
        let mut builds = 0u32;
        let mut build = |lru: &mut Lru<u32>, key: &str| {
            lru.get_or_insert_with((String::from("hash"), key.to_string()), || {
                builds += 1;
                builds
            })
        };
        assert_eq!(build(&mut lru, "a"), 1);
        assert_eq!(build(&mut lru, "b"), 2);
        assert_eq!(build(&mut lru, "a"), 1, "a hit answers the held value");
        assert_eq!(build(&mut lru, "c"), 3, "the third key builds");
        assert_eq!(lru.len(), 2, "the cap holds");
        assert_eq!(build(&mut lru, "a"), 1, "the recently used entry survived");
        assert_eq!(
            build(&mut lru, "b"),
            4,
            "the least recently used one was evicted and rebuilds"
        );
    }
}
