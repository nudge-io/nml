//! Discovery and file resolution (RFC 0019 item 0, step 0c; Layer B —
//! reads texts, loads packages, speaks `Diagnostic`).
//!
//! [`discover`] is a BOUNDED downward walk from the root (65,536 entries;
//! policy skips for `.`-dirs, `node_modules`, `target`; an exact skip
//! below depth 64, where nothing is keyable; symlinks never descended nor
//! loaded) that collects every manifest, project config and root marker
//! INTERLEAVED with the settlement — list depth *d*, settle key depth
//! *d+1*, list depth *d+1* — and settles liveness by INDUCTION ON
//! DEPTH (E26 R3′): an input at `p`
//! is inert iff a live workspace manifest whose directory is a strict
//! ancestor of `p` claims it — liveness is a function of strictly
//! shallower inputs only, so `(depth, path)` is presentation order, never
//! authority. Inertness is settled BEFORE anything is read (E28): an
//! inert manifest is never loaded — its text, its declared sources and
//! its marker names are all dead — so a tenant cannot fail, hang or
//! spam the operator's checks with content the operator's binding
//! claims. Anchors are manifest-derived (R4). Truncation is LOUD and
//! fail-closed (A16): error-severity, naming the directory where the
//! walk stopped; the universe is closed-denied, never "no claims".
//! The entry bound is charged per TENANT-SHAPED BUDGET UNIT (the
//! directories at a claiming glob's unit boundary — the start of its
//! last run of wildcard directory segments): one tenant exhausting its
//! unit denies its own subtree and no other, while the root unit and
//! the [`MAX_TOTAL_ENTRIES`] backstop keep the whole-universe
//! truncation where it always was. The live-input byte budget
//! ([`MAX_LIVE_INPUT_BYTES`]) and an unlistable directory are charged
//! per unit the same way. A live manifest that sits inside content a
//! strictly shallower live claim reaches — under a unit root, or in a
//! gap of the operator's glob — mints no unit anywhere, and the bytes
//! the whole walk reads are backstopped by
//! [`MAX_TOTAL_LIVE_INPUT_BYTES`] exactly as its entries are by
//! [`MAX_TOTAL_ENTRIES`].
//!
//! [`resolve_file`] mints the checked file's key under the universe's
//! trust and turns the kernel's typed rejections into NML2083.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use nml_core::diagnostic::Diagnostic;
use nml_core::project::ProjectConfig;

use crate::package::{PackageError, SchemaPackage, check_plain_file_name};
use crate::schema::SchemaValidator;

use super::claims::{
    ClaimClass, Closure, ExternalClaim, Governing, ManifestClaim, ProjectConfigClaim, UnitBound,
    UnitTruncation, Universe, ValidatorMemo, claiming_outer, governing, is_budget_unit,
    reaching_outer,
};
use super::diag;
use super::grants::Grant;
use super::paths::{PathError, SourceKey, SymlinkVerdict, Trust, WorkspaceRoot};
use crate::file_names::{
    PROJECT_CONFIG_NAME, is_manifest_name, is_nml_name, is_nml_name_bytes, manifest_stem,
};
use crate::fs::{
    EntryKind, FsError, LstatFs, MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES, PathFs, read_beneath,
};
use crate::glob::is_plain_name;

/// Entry bound on the discovery walk. Policy, not cost (60k entries walk
/// in well under a second): raising it is the owner's call; reaching it
/// is an error, never a silent narrowing.
///
/// Settlement probes count as entries: every `reaches` or
/// `is_budget_unit` question the walk asks of a candidate claim while
/// settling an input or deciding a unit root is charged to the same
/// budget, through `EntryBudget`, so the bound is on the walk's WORK
/// and not on its listings alone.
///
/// LIMIT: reach=content guards=work surface=kernel shown="65536" — directory entries one BUDGET UNIT (a directory at a claiming glob's unit boundary — the start of its last run of wildcard directory segments — else the root) may spend before the walk truncates that unit alone (NML2089)
pub const MAX_ENTRIES: usize = 65_536;

/// The universe-wide BACKSTOP on the walk (A16 amendment): the sum
/// of every budget unit's spend and the root unit's. [`MAX_ENTRIES`]
/// bounds one unit; this bounds the walk as a whole, so a layout that
/// mints many units cannot multiply the per-unit budget into an
/// unbounded walk. Past it the universe truncates exactly as the single
/// cumulative bound did before the amendment. Sixteen units' worth: an
/// operator with more than sixteen full tenants is past the point where
/// one root is the right unit of checking.
///
/// LIMIT: reach=content guards=work surface=kernel shown="1048576" — directory entries one universe walk visits in total, every budget unit summed, before the WHOLE universe is truncated (NML2089)
pub const MAX_TOTAL_ENTRIES: usize = 16 * MAX_ENTRIES;

/// What a discovery read is FOR — the caller's reader bounds each kind on
/// its own (E28: the CLI caps a manifest or a project config far below a
/// declared schema source, because a cap bounds memory, not parse time,
/// and a live resolution input is parsed on every invocation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputKind {
    /// A live `*.package.nml`.
    Manifest,
    /// A live `nml-project.nml`.
    ProjectConfig,
    /// A schema source a live manifest declares.
    Source,
}

impl InputKind {
    /// The kind a discovery input with this FILE NAME is read as — the
    /// kernel's one name rule (`is_manifest_name`, `PROJECT_CONFIG_NAME`),
    /// the same one the walk classifies by. A front end that reads such a
    /// file outside the walk (the editor's code-action reader) asks HERE
    /// rather than re-spelling the suffixes: the editor's own copy read a
    /// bare `package.nml` — which is deliberately NOT a manifest — under
    /// the 256 KiB manifest cap the walk would never apply to it.
    pub fn of_name(name: &str) -> Self {
        if is_manifest_name(name) {
            Self::Manifest
        } else if name == PROJECT_CONFIG_NAME {
            Self::ProjectConfig
        } else {
            Self::Source
        }
    }

    /// The noun a read failure names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Manifest => "package manifest",
            Self::ProjectConfig => "project config",
            Self::Source => "declared schema source",
        }
    }
}

/// The byte bound one discovery input KIND reads under (E28, E29(3)) —
/// the kernel's contract, so every front end (the CLI's reader today,
/// the editor's in step 0e) refuses an oversized input at the same
/// size. A cap bounds memory, not time: the parser is linear, and a
/// LIVE resolution input is re-parsed on every invocation of every
/// verb, so the bound is a policy on what a manifest or a project
/// config may reasonably be — a declaration list, 256 KiB — while a
/// declared schema source keeps the 4 MiB a schema file is allowed.
/// Past the cap the read is a refusal naming the kind and the bound,
/// never the memory. The check TARGET's own cap is the CLI's
/// (`MAX_TARGET_BYTES`): the target is an invocation input, not a
/// discovery input.
/// UNPUBLISHED: a selector over MAX_MANIFEST_BYTES and MAX_SOURCE_BYTES, which are the published rows
pub const fn input_cap(kind: InputKind) -> usize {
    match kind {
        InputKind::Manifest | InputKind::ProjectConfig => MAX_MANIFEST_BYTES,
        InputKind::Source => MAX_SOURCE_BYTES,
    }
}

/// The caller's text reader — buffer-first in the editor, byte-capped per
/// [`InputKind`] ([`input_cap`]) in every front end. The error is the
/// detail a load failure reports.
pub type ReadText<'a> = &'a dyn Fn(InputKind, &Path) -> Result<String, String>;

/// THE disk case of [`ReadText`], for every front end: a discovery input
/// the walk classified — always under `root` — read through the kernel's
/// one reader ([`read_beneath`]: the race-free chain, so a directory
/// swapped for a link between the walk's `lstat` and this read is
/// refused, never followed) under its kind's cap ([`input_cap`]). The
/// refusal is the kernel's own sentence, the one both front ends print
/// for the same file: the cap sentence, `not UTF-8`, or the open's
/// refusal as a path-based read would have spelled it. A path that is
/// not under the root is refused before any open: the walk never hands
/// one out, and the reader never trusts its caller. The editor answers
/// from its open buffers FIRST and comes here for the rest; a one-shot
/// run comes here for everything.
pub fn read_input(root: &WorkspaceRoot, kind: InputKind, path: &Path) -> Result<String, String> {
    let rel = path
        .strip_prefix(root.path())
        .map_err(|_| "path is not under the workspace root".to_string())?;
    let components: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str().unwrap_or(""))
        .collect();
    read_beneath(
        root.path(),
        &components,
        input_cap(kind),
        &format!("a {}", kind.label()),
    )
    .map_err(|e| e.to_string())
}

/// The bound on live-input bytes PER BUDGET UNIT: every live manifest's
/// own text, every live project config's and every DISTINCT declared
/// source, summed over the unit's
/// inputs in one discovery — the root unit's sum is the universe's.
/// [`input_cap`] bounds one input; this bounds the set of them, which
/// is the quantity a tenant actually controls — the entry bound admits
/// tens of thousands of live inputs per unit, and 500 manifests
/// declaring one 3.99 MiB source measured 1,927 MB of resident text
/// (the per-source cache below removes the ×N; this removes the ×N of
/// DISTINCT sources, which no cache can). Past it the unit is denied
/// exactly as the entry bound denies it — closed, loud, nothing else
/// moving — and the root unit's exhaustion is the whole universe,
/// closed-denied in full, never a partial universe.
///
/// 64 MiB = sixteen maximal (4 MiB) schema sources, or 256 maximal
/// (256 KiB) manifests or project configs — orders of magnitude above
/// any repository that is one checking root (this one's whole
/// live-input set is 12 KiB) and two orders below the runner memory the
/// unbounded form reaches. The walk as a whole is bounded by
/// [`MAX_TOTAL_LIVE_INPUT_BYTES`].
///
/// LIMIT: reach=content guards=memory surface=kernel shown="64 MiB" — bytes of live manifests, live project configs and DISTINCT declared schema sources one BUDGET UNIT may read before the unit is denied (NML2089); the root unit's are the universe's
pub const MAX_LIVE_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// The universe-wide BACKSTOP on live-input bytes: the sum of every
/// budget unit's live-input bytes and the root
/// unit's — the byte twin of [`MAX_TOTAL_ENTRIES`]. [`MAX_LIVE_INPUT_BYTES`]
/// bounds one unit; this bounds what one discovery reads and retains in
/// all, so a layout that mints many units cannot multiply the per-unit
/// budget into an unbounded resident set (measured: a tenant
/// minting eight units made every `nml check` read and hold 512 MiB of
/// its sparse sources, silently, with the universe `Complete`). Past it
/// the universe truncates in full — `Truncation::TotalLiveInputBytes`,
/// naming the input that crossed it — exactly as the entry backstop
/// does. Sixteen units' worth, the entry doctrine applied to bytes: an
/// exhausted unit contributes its bytes up to and including the input
/// that crossed its own bound, so sixteen spent units deny everyone.
///
/// LIMIT: reach=content guards=memory surface=kernel shown="1 GiB" — bytes of live manifests, live project configs and DISTINCT declared schema sources one universe walk reads in total, every budget unit summed, before the WHOLE universe is truncated (NML2089)
pub const MAX_TOTAL_LIVE_INPUT_BYTES: usize = 16 * MAX_LIVE_INPUT_BYTES;

/// One budget unit's live-input accounting (one per unit, the root
/// unit's being the universe's).
#[derive(Default)]
struct LiveInputs {
    /// Declared source texts by key: read ONCE per `(directory, file)`
    /// and shared by every live manifest that declares it (a declared
    /// source sits beside its manifest, so its key's unit is the
    /// manifest's — one cache per unit loses no sharing).
    sources: BTreeMap<SourceKey, Arc<str>>,
    /// Manifest and config texts plus DISTINCT source texts, in bytes.
    bytes: usize,
}

/// The walk's live workspace claims: every settled claim in
/// SETTLEMENT order — (key depth, key), the order `reaching_outer`
/// relies on — and the OPERATOR-LEVEL ones beside them, because the
/// unit-root question is asked once per listed directory and must cost
/// the operator's claims, never every live claim a tenant committed.
#[derive(Default)]
struct LiveClaims {
    all: Vec<ManifestClaim>,
    operator: Vec<ManifestClaim>,
}

/// One charge's verdict — bytes ([`LiveInputs::charge`]) and entries
/// ([`EntryBudget`]) alike.
enum Charge {
    Admitted,
    /// The unit's own bound ([`MAX_LIVE_INPUT_BYTES`], [`MAX_ENTRIES`])
    /// is spent.
    UnitSpent,
    /// The universe-wide backstop ([`MAX_TOTAL_LIVE_INPUT_BYTES`],
    /// [`MAX_TOTAL_ENTRIES`]) is crossed.
    Backstop,
}

/// The walk's ENTRY budgets — [`MAX_ENTRIES`] per budget unit and the
/// [`MAX_TOTAL_ENTRIES`] backstop — at ONE chokepoint: every listed
/// directory entry and every settlement probe (a `reaches` or
/// `is_budget_unit` question asked of a candidate claim) is charged here,
/// so a tenant's live manifests cannot buy unbounded settlement WORK any
/// more than unbounded listing. No new bound: a probe is an entry. Before
/// this, N sibling manifests whose globs reach nothing (`zzz/*.nml`) over
/// N inputs beneath them cost N² uncharged probes per invocation
/// (the audit's residual), in the gap of an operator's glob or under an
/// unclaimed subtree.
struct EntryBudget {
    unit_bound: usize,
    total_bound: usize,
    /// Spend per budget unit, under the unit root's key — the root
    /// unit's under the root key.
    spent: BTreeMap<SourceKey, usize>,
    total: usize,
}

/// One unit's open ledger: the unit's counter and the universe's, for
/// a burst of charges (a directory listing) without a map lookup each.
struct Ledger<'a> {
    spent: &'a mut usize,
    total: &'a mut usize,
    unit_bound: usize,
    total_bound: usize,
}

impl Ledger<'_> {
    /// Saturating, like the byte budgets: the sums are bounds, not
    /// statistics. The backstop's verdict wins when both are crossed.
    fn charge(&mut self, n: usize) -> Charge {
        *self.spent = self.spent.saturating_add(n);
        *self.total = self.total.saturating_add(n);
        if *self.total > self.total_bound {
            Charge::Backstop
        } else if *self.spent > self.unit_bound {
            Charge::UnitSpent
        } else {
            Charge::Admitted
        }
    }
}

impl EntryBudget {
    fn new() -> Self {
        Self::bounded(MAX_ENTRIES, MAX_TOTAL_ENTRIES)
    }

    fn bounded(unit_bound: usize, total_bound: usize) -> Self {
        Self {
            unit_bound,
            total_bound,
            spent: BTreeMap::new(),
            total: 0,
        }
    }

    /// The ledger of `unit` (`None` is the root unit).
    fn ledger(&mut self, unit: Option<&SourceKey>) -> Ledger<'_> {
        Ledger {
            spent: self.spent.entry(budget_key(unit)).or_insert(0),
            total: &mut self.total,
            unit_bound: self.unit_bound,
            total_bound: self.total_bound,
        }
    }

    /// Charge `n` entries or probes to `unit`.
    fn charge(&mut self, unit: Option<&SourceKey>, n: usize) -> Charge {
        self.ledger(unit).charge(n)
    }
}

/// The walk's live-input BYTE bounds — [`MAX_LIVE_INPUT_BYTES`] per
/// budget unit and the [`MAX_TOTAL_LIVE_INPUT_BYTES`] backstop — CARRIED
/// like the entry bounds ([`EntryBudget`]) and not read from the
/// constants at the charge, so the default test lane has the same seam
/// for the byte backstop that it has for the entry one
/// ([`discover_byte_scaled`]): the universe-wide byte bound is a
/// gigabyte, which no default-lane test can reach and which therefore
/// went unpinned. Never a runtime knob.
#[derive(Debug, Clone, Copy)]
struct ByteBounds {
    unit: usize,
    total: usize,
}

impl ByteBounds {
    fn new() -> Self {
        Self {
            unit: MAX_LIVE_INPUT_BYTES,
            total: MAX_TOTAL_LIVE_INPUT_BYTES,
        }
    }
}

impl LiveInputs {
    /// Charge `n` bytes against the unit's [`ByteBounds::unit`] and the
    /// universe's [`ByteBounds::total`] (`total`, every unit summed).
    /// Saturating: the sums are bounds, not statistics, and must not
    /// wrap. The backstop's verdict wins when one input crosses both.
    fn charge(&mut self, n: usize, total: &mut usize, bounds: ByteBounds) -> Charge {
        self.bytes = self.bytes.saturating_add(n);
        *total = total.saturating_add(n);
        if *total > bounds.total {
            Charge::Backstop
        } else if self.bytes > bounds.unit {
            Charge::UnitSpent
        } else {
            Charge::Admitted
        }
    }
}

/// Why one live manifest did not load.
enum LoadFailure {
    /// THIS manifest is unloadable (NML2088): the universe carries on
    /// closed-denied around it.
    Input(String),
    /// THIS manifest failed to parse, meta-validate or satisfy a loader
    /// rule — typed, so a finding with a code of its own (NML2081) is
    /// reported under it ([`diag::manifest_unloadable`]) — with the
    /// text it was read from, kept so the row's span locates.
    Manifest { err: PackageError, text: Arc<str> },
    /// The unit's live-input budget is spent (A16): the unit is denied
    /// — the root unit's is the whole universe — naming the input that
    /// crossed the bound.
    Budget(SourceKey),
    /// The universe-wide byte backstop is crossed: the universe
    /// is denied in full, naming the input that crossed it.
    Backstop(SourceKey),
}

/// The build-product directories (`node_modules`, `target`): never
/// claimable, never descended into by the walk, never audited — the
/// one list both predicates share.
pub(crate) fn policy_dir(name: &str) -> bool {
    name == "node_modules" || name == "target"
}

/// The policy skip list — directory names the walk never descends into
/// (never claimable): the hidden ones and the build products. The
/// editor's watcher filter uses this same predicate; its index is the
/// walk's own enumeration.
pub fn walk_skips_dir(name: &str) -> bool {
    name.starts_with('.') || policy_dir(name)
}

/// Why the walk was cut short (A16). Error-severity for every verb: a
/// universe whose manifests cannot be enumerated validates NOTHING (it is
/// closed and denies every file, exit 1) — it never degrades to
/// parse-only checking with a green exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Truncation {
    /// The entry bound was reached while listing `dir`, or settling an
    /// input in it (settlement probes are charged as entries).
    Entries { dir: SourceKey },
    /// `dir` could not be listed.
    Unreadable { dir: SourceKey, error: FsError },
    /// The ROOT unit's live-input byte budget was spent reading the
    /// live input at `key` (a unit's own budget is its
    /// [`UnitTruncation`]).
    LiveInputBytes { key: SourceKey },
    /// The universe-wide live-input backstop
    /// ([`MAX_TOTAL_LIVE_INPUT_BYTES`], every unit summed) was crossed
    /// reading the live input at `key`.
    TotalLiveInputBytes { key: SourceKey },
}

/// What one root's discovery found.
#[derive(Debug)]
pub struct Discovery {
    /// The root this discovery walked: the universe it yields
    /// ([`Self::universe`]) is THIS root's by construction — read through
    /// [`Self::root`], never reassigned, so a discovery read under another
    /// root is unrepresentable (the front ends used to carry the pair and
    /// hand the root back).
    pub(crate) root: WorkspaceRoot,
    pub(crate) claims: Vec<ManifestClaim>,
    /// The live project configs, read through [`Universe::nearest_config`].
    pub(crate) configs: Vec<ProjectConfigClaim>,
    /// NML2080 per inert input.
    pub(crate) inert: Vec<Diagnostic>,
    /// The walk was cut short: the universe is closed-denied in full —
    /// nothing was loaded, nothing binds, and the truncation is the one
    /// (error-severity) note.
    pub(crate) truncated: Option<Truncation>,
    /// Budget units the walk stopped inside (A16 amendment): the entry
    /// bound, the unit's live-input byte budget or an unlistable
    /// directory ([`UnitTruncation::why`]) — every key under one is
    /// denied, and nothing else is. Nothing under a truncated unit is
    /// KEPT: the walk stops listing it, its files leave
    /// [`Discovery::files`] and its notes leave [`Discovery::inert`] and
    /// `load_errors` (the universe's errors); a live input settled under it before
    /// the bound was reached was read (its bytes counted) and is dropped
    /// from the claims and configs.
    pub(crate) truncated_units: Vec<UnitTruncation>,
    /// Live manifests and configs that failed to load (NML2088),
    /// attributed by key — read through [`Self::universe_errors`].
    pub(crate) load_errors: Vec<Diagnostic>,
    /// The text of every live manifest that failed to load, by key —
    /// kept (bounded by the manifest byte cap, as a loaded package's
    /// own text is) so the row spanned at its first finding locates
    /// ([`Self::manifest_location`]).
    pub(crate) failed_manifests: Vec<(SourceKey, Arc<str>)>,
    /// Every regular file the walk saw, by depth then key — the one
    /// enumeration of the root: a front end
    /// that needs "which files are under this root" (the editor's
    /// vocabulary coverage, its index) reads this instead of walking a
    /// second time under a second bound. Links, directories and policy-
    /// skipped subtrees are not in it; a truncated walk leaves it empty
    /// (the universe is denied in full, and a partial listing would read
    /// as a whole one). Read through [`Self::nml_files_under`] by every
    /// front end that expands a directory (`nml fix`, the checking
    /// verbs, the editor's index) and by the editor's coverage question.
    pub(crate) files: Vec<SourceKey>,
    /// What the walk left OUT of [`Self::files`] by policy, by depth then
    /// key: every symlink it saw (a link is never descended or read,
    /// whatever it points at), every `.nml`-named FIFO, socket or
    /// device, every `.nml` dot-file, every directory it did not
    /// enter — a dot-directory, or the policy-skipped `node_modules` and
    /// `target` ([`walk_skips_dir`]) — every entry whose name no key can
    /// carry ([`Skip::UnkeyableName`]) and every directory at the
    /// component bound ([`Skip::ComponentBound`]). A gate that certifies a tree
    /// reports these, so content the walk never judged is never silently
    /// certified; nothing under a spent unit or a truncated
    /// universe is kept here, for the reason nothing is kept in `files`.
    pub(crate) skipped: Vec<Skipped>,
    /// The per-root coverage answer (`coverers`), computed once
    /// on first ask — `governing` per discovered file is O(files × live
    /// claims), and a universe is asked far more often than it is rebuilt.
    pub(crate) coverers: OnceLock<Vec<(String, ClaimClass)>>,
    /// The validators this universe's bindings build, once each
    /// ([`ValidatorMemo`]): shared behind an `Arc` so a front end that
    /// keeps universes alive across rediscoveries (the editor) hands
    /// every discovery ONE table and a rediscovered manifest with an
    /// unchanged hash costs no second build.
    pub(crate) validators: Arc<ValidatorMemo>,
}

/// What a gate learns about a skipped dot-directory — LISTED only,
/// never settled, keyed but never part of the universe — within the
/// run's [`AuditBudget`] and
/// [`MAX_COMPONENTS`](super::paths::MAX_COMPONENTS) components, so a
/// tenant's hidden tree costs a bounded audit and no new bound. `.git`
/// is never audited (VCS metadata, the fence's own entry, never
/// content), nor are the policy directories beneath. What it learns is
/// how many `.nml` entries the directory holds — files, links and FIFOs
/// alike: content a runtime could read that no verb judged — the first
/// [`MAX_AUDIT_EXAMPLES`] of their keys by depth then name, and whether
/// the audit finished. A COUNT and examples, never every key: the
/// gate's memory is O(hidden directories), not O(hidden files) — a
/// tenant's committed dot-directory of 300,000 `.nml` files used to
/// cost the gate 300,000 rows (~1 KB each, held and deduplicated).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HiddenAudit {
    /// `.nml`-named entries beneath the directory, exactly.
    pub nml: usize,
    /// The first [`MAX_AUDIT_EXAMPLES`] of them, by depth then key.
    pub examples: Vec<SourceKey>,
    /// The audit stopped short — the run's budget spent, a directory
    /// beneath whose name no key can carry or that sits at the component
    /// bound (never listed), or an
    /// unlistable directory — at this directory, leaving content beneath
    /// it unaudited.
    pub incomplete: Option<SourceKey>,
}

/// Example keys an NML2090 hidden-directory row names beside its exact
/// count: enough to show WHAT is hidden, never the whole listing — the
/// gate's memory is the count and these, per directory.
///
/// LIMIT: reach=content guards=output surface=kernel shown="8" — example keys a hidden-directory NML2090 row names beside its exact count
pub const MAX_AUDIT_EXAMPLES: usize = 8;

/// The entries ONE run's hidden audits may list, all skipped
/// dot-directories together: [`MAX_TOTAL_ENTRIES`], the universe's own
/// backstop — no new bound — so a developer's `.venv` or `.next`
/// (tens of thousands of entries) audits whole under `nml check .`
/// while a hidden flood still meets a bound and is reported
/// incomplete, an error the gate keeps. The caller holds one per run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditBudget {
    remaining: usize,
}

impl Default for AuditBudget {
    fn default() -> Self {
        Self::of(MAX_TOTAL_ENTRIES)
    }
}

impl AuditBudget {
    /// A budget of `entries` listings (tests; the run's is `default`).
    pub(crate) fn of(entries: usize) -> Self {
        Self { remaining: entries }
    }
}

/// Audit the skipped dot-directory `dir` for a gate ([`HiddenAudit`]),
/// spending the run's `budget`.
pub fn audit_hidden(
    root: &WorkspaceRoot,
    fs: &dyn LstatFs,
    dir: &SourceKey,
    budget: &mut AuditBudget,
) -> HiddenAudit {
    let mut audit = HiddenAudit::default();
    if dir.file_name() == ".git" {
        return audit;
    }
    // Breadth-first over the oracle's listings, which are SORTED by the
    // [`LstatFs::list_dir`] contract (every backend sorts at that one
    // boundary, as the walk relies on): the examples come out by depth
    // then key without holding every key to sort — the frontier holds
    // directories only.
    let mut frontier = std::collections::VecDeque::from([dir.clone()]);
    while let Some(d) = frontier.pop_front() {
        let entries = match fs.list_dir(&root.path_of(&d)) {
            Ok(entries) => entries,
            Err(_) => {
                // Stopped short HERE, not everywhere: the first directory
                // the audit could not list is `incomplete` (the gate's
                // own row), and the rest of the frontier is still listed,
                // so the count is everything the gate could see — a lower
                // bound the row says is one.
                audit.incomplete.get_or_insert(d);
                continue;
            }
        };
        for (name, kind) in entries {
            if budget.remaining == 0 {
                audit.incomplete = Some(d);
                return audit;
            }
            budget.remaining -= 1;
            // A name no key can carry: a directory beneath it is never
            // listed — the audit is INCOMPLETE here and the count a lower
            // bound — and a `.nml`-named entry is counted (no key to show
            // as an example).
            let name = match listed(&name, kind) {
                Some(Listed::Plain(plain)) => plain,
                Some(Listed::Unkeyable { kind, name }) => {
                    if kind == EntryKind::Dir {
                        audit.incomplete.get_or_insert(d.clone());
                    } else if is_nml_name(&name) {
                        audit.nml += 1;
                    }
                    continue;
                }
                None => continue,
            };
            let nml = is_nml_name(name);
            match kind {
                EntryKind::Dir => {
                    if name == ".git" || policy_dir(name) {
                        continue;
                    }
                    // Never listed past the component bound: the audit
                    // is incomplete here, the count a lower bound.
                    match d.child_dir(name) {
                        Some(sub) => frontier.push_back(sub),
                        None => {
                            audit.incomplete.get_or_insert(d.clone());
                        }
                    }
                }
                EntryKind::File | EntryKind::Symlink | EntryKind::Other if nml => {
                    audit.nml += 1;
                    if audit.examples.len() < MAX_AUDIT_EXAMPLES {
                        audit.examples.push(d.join(name));
                    }
                }
                EntryKind::File | EntryKind::Symlink | EntryKind::Other => {}
            }
        }
    }
    audit
}

/// An entry the walk saw and left out of [`Discovery::files`] by policy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Skipped {
    /// The entry's key — or, for an entry whose name no key can carry
    /// ([`Skip::UnkeyableName`], which carries the name), the key of the
    /// directory holding it.
    pub key: SourceKey,
    pub why: Skip,
}

impl Skipped {
    /// The entry's own name where `key` cannot carry it — the wire's
    /// `entry` property, present exactly for [`Skip::UnkeyableName`] (the
    /// name is that variant's own field, so the property's presence is a
    /// fact of the type, not an invariant a row has to keep).
    pub fn entry(&self) -> Option<&str> {
        match &self.why {
            Skip::UnkeyableName { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// Why the walk left an entry out — one lowerCamel word each on the
/// `--json` wire ([`Skip::tag`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Skip {
    /// A symlink of any name: never descended, never read, its target
    /// never resolved.
    Symlink,
    /// A `.nml`-named entry that is neither a file, a directory nor a
    /// link — a FIFO, a socket, a device.
    Fifo,
    /// A directory whose name starts with `.` — never entered.
    DotDirectory,
    /// A `.nml` file whose name starts with `.` — listed, never
    /// enumerated (never fixed, checked or indexed unasked).
    DotFile,
    /// `node_modules` or `target` — never entered.
    PolicyDirectory,
    /// An entry whose NAME no key can carry — not UTF-8, or bearing a
    /// separator (`a\b` on unix: a legal name git tracks) — under the
    /// directory the row's key names: charged, never joined, never
    /// entered, never enumerated. Reported with its `lstat` kind, so a
    /// gate can say what it could not judge: a directory (content
    /// beneath it, if any, unlisted), a symlink, or a `.nml`-named file
    /// or special entry (a `.txt` with the same defect is no content and
    /// no row). A silently dropped name certified a tree the gate never
    /// looked at whole: a tenant's `ev\il/hidden.flow.nml` under the
    /// operator's glob passed `nml check .` unjudged, exit 0. `name` is
    /// the entry's own, spelled lossily (a non-UTF-8 byte is U+FFFD).
    UnkeyableName { kind: EntryKind, name: String },
    /// A directory at the component bound ([`MAX_COMPONENTS`](super::paths::MAX_COMPONENTS)):
    /// never listed — nothing beneath it is keyable, so the skip is exact
    /// and never a truncation — and REPORTED at its own key (the last
    /// keyable depth), since content beneath it is unjudged: 63 nested
    /// directories hid a tenant's `.flow.nml` from `nml check .` (exit 0)
    /// while naming it was refused as too deep.
    ComponentBound,
}

impl Skip {
    /// The wire's word: `symlink`, `fifo`, `dotDirectory`, `dotFile`,
    /// `policyDirectory`, `unkeyableName`, `componentBound`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Symlink => "symlink",
            Self::Fifo => "fifo",
            Self::DotDirectory => "dotDirectory",
            Self::DotFile => "dotFile",
            Self::PolicyDirectory => "policyDirectory",
            Self::UnkeyableName { .. } => "unkeyableName",
            Self::ComponentBound => "componentBound",
        }
    }

    /// The policy word for a directory the walk does not enter.
    fn for_dir(name: &str) -> Self {
        if name.starts_with('.') {
            Self::DotDirectory
        } else {
            Self::PolicyDirectory
        }
    }
}

/// A listed entry's name as the walk and the hidden audit read it — the
/// ONE place a name becomes a key component or a reported skip, so the
/// two loops cannot drift (each used to spell the plain-name test and
/// the unkeyable rule itself, and one of them dropped the entry).
enum Listed<'n> {
    /// A plain name: a key component ([`SourceKey::join`]).
    Plain(&'n str),
    /// A name no key can carry that holds content or may hold some — a
    /// directory, a link, a `.nml`-named entry — with its `lstat` kind
    /// and the name spelled lossily: the walk reports it under the
    /// holding directory ([`Skip::UnkeyableName`]); the audit is
    /// incomplete at a directory and counts a `.nml` name.
    Unkeyable { kind: EntryKind, name: String },
}

/// `None` is the one INERT case: a name no key can carry that is neither
/// a container nor `.nml`-named (`a\b.txt`) — no content, no row.
fn listed(name: &OsStr, kind: EntryKind) -> Option<Listed<'_>> {
    match name.to_str() {
        Some(plain) if is_plain_name(plain) => Some(Listed::Plain(plain)),
        _ => {
            let nml = is_nml_name_bytes(name.as_encoded_bytes());
            (matches!(kind, EntryKind::Dir | EntryKind::Symlink) || nml).then(|| {
                Listed::Unkeyable {
                    kind,
                    name: name.to_string_lossy().into_owned(),
                }
            })
        }
    }
}

impl Discovery {
    /// Nothing found yet under `root`, sharing `validators` — the table
    /// is an INPUT to discovery (the editor hands every discovery its
    /// one table; the CLI a fresh one), never a field a caller swaps.
    fn empty(root: WorkspaceRoot, validators: Arc<ValidatorMemo>) -> Self {
        Self {
            root,
            claims: Vec::new(),
            configs: Vec::new(),
            inert: Vec::new(),
            truncated: None,
            truncated_units: Vec::new(),
            load_errors: Vec::new(),
            failed_manifests: Vec::new(),
            files: Vec::new(),
            skipped: Vec::new(),
            coverers: OnceLock::new(),
            validators,
        }
    }

    /// The root this discovery walked — the one root every key, path and
    /// universe of it is spelled against.
    pub fn root(&self) -> &WorkspaceRoot {
        &self.root
    }

    /// The live manifest claims this walk discovered, in depth order.
    /// Read-only, like every other product of the walk: a `Discovery` is
    /// what the kernel SAW, and a caller that could push, clear or
    /// reorder these would contradict the memo derived from them
    /// ([`Self::vocabulary_for`]'s per-root coverage answer is computed
    /// ONCE, from exactly this list) — the same door
    /// [`Self::root`] and [`Self::validators`] already closed.
    pub fn claims(&self) -> &[ManifestClaim] {
        &self.claims
    }

    /// NML2080 per inert input — the inputs the walk read as content.
    pub fn inert(&self) -> &[Diagnostic] {
        &self.inert
    }

    /// The universe-wide truncation, if the walk was cut short.
    pub fn truncated(&self) -> Option<&Truncation> {
        self.truncated.as_ref()
    }

    /// The budget units the walk stopped inside (A16): every key under
    /// one is denied, and nothing else is.
    pub fn truncated_units(&self) -> &[UnitTruncation] {
        &self.truncated_units
    }

    /// Every regular file the walk saw, by depth then key — THE
    /// enumeration of the root. A front end that wants the files under
    /// one directory reads [`Self::nml_files_under`], which applies the
    /// walk's own name rule.
    pub fn files(&self) -> &[SourceKey] {
        &self.files
    }

    /// What the walk left OUT of [`Self::files`] by policy, by depth
    /// then key — the rows a gate that certifies a tree must report.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// The validator table the universe's bindings build into — the one
    /// handed to [`discover`], never swapped after.
    pub fn validators(&self) -> &Arc<ValidatorMemo> {
        &self.validators
    }

    /// The text of a live manifest this discovery read, by its key: a
    /// loaded package's, or one that failed to load (kept so its row
    /// locates); `None` for any other source.
    pub fn manifest_text(&self, key: &str) -> Option<&str> {
        self.claims
            .iter()
            .find(|c| c.manifest().is_some_and(|m| m.as_str() == key))
            .map(|c| c.package.manifest_text.as_str())
            .or_else(|| {
                self.failed_manifests
                    .iter()
                    .find(|(k, _)| k.as_str() == key)
                    .map(|(_, text)| &**text)
            })
    }

    /// Where `span` sits in the text of the live manifest keyed `source`
    /// (1-based line and column): the ONE derivation behind every place
    /// a front end shows in a manifest — a row's own span
    /// ([`Self::manifest_location`]) and every place a front end locates
    /// in it, a note's and a cause's, alike — over the bytes this discovery
    /// judged: a loaded package's text, or a failed manifest's kept for
    /// this; never a second read, which could see other bytes. `None`
    /// for a source that is no live manifest, or a span past the text (a
    /// stale offset is never indexed).
    pub fn locate(
        &self,
        source: &str,
        span: nml_core::span::Span,
    ) -> Option<nml_core::span::Location> {
        crate::package::locate_in(self.manifest_text(source)?, span)
    }

    /// Where a row spanned in a manifest's text sits: [`Self::locate`]
    /// over the row's own `source` and `span` — the CLI's
    /// `key:line:col:` and its `--json` `line`/`col` for a row whose
    /// `source` is a live manifest's key (loaded or not: NML2092 at a
    /// glob, NML2081 at an item, NML2088 at a manifest's first finding).
    /// `None` for a foreign source, a span past the text, or no span at
    /// all — the row then prints locationless.
    pub fn manifest_location(&self, diag: &Diagnostic) -> Option<nml_core::span::Location> {
        self.locate(diag.source.as_deref()?, diag.span?)
    }

    /// The universe this discovery found under its own root.
    pub fn universe(&self) -> Universe<'_> {
        let closure = if self.truncated.is_some() {
            Closure::Truncated
        } else if self.load_errors.is_empty() {
            Closure::Complete
        } else {
            Closure::Unloadable(self.load_errors.len())
        };
        Universe {
            root: &self.root,
            claims: &self.claims,
            configs: &self.configs,
            closure,
            truncated_units: &self.truncated_units,
            validators: &self.validators,
        }
    }

    /// A16: the truncation ERROR (NML2089, minted by the A7 table),
    /// naming the directory where the walk stopped. The FACT only — the
    /// `--root` advice is the CLI reporter's sentence (E35).
    pub(crate) fn truncation_error(&self) -> Option<Diagnostic> {
        self.truncated.as_ref().map(diag::universe_truncated)
    }

    /// THE enumeration every front end expands a directory to — `nml
    /// fix`'s directory arguments, the checking verbs' directory targets,
    /// the editor's index: the `.nml` files the walk saw under `dir` (a
    /// directory key, or the root key), by depth then key. Regular files
    /// only, and never a dot-file: a symlink, a FIFO, a policy-skipped
    /// subtree ([`walk_skips_dir`]) and a file the filesystem hides by
    /// convention (`.hidden.nml`) are not among them — a hidden file is
    /// never rewritten, checked or indexed unasked; named on a command
    /// line it is the operator's. A truncated universe enumerates nothing
    /// ([`Self::files`] is empty), and a spent budget unit's files are
    /// absent.
    pub fn nml_files_under<'a>(
        &'a self,
        dir: &'a SourceKey,
    ) -> impl Iterator<Item = &'a SourceKey> + 'a {
        self.files.iter().filter(move |key| {
            dir.contains(key) && is_nml_name(key.file_name()) && !key.file_name().starts_with('.')
        })
    }
}

/// Load a LIVE workspace manifest: its declared sources are plain names
/// next to it, each `lstat`-checked through the oracle BEFORE it is read
/// — a declared source that is not a regular file (a symlink, to a FIFO
/// say; a device) is refused, never opened. The file's STEM must equal
/// the declared `name` (RFC 0030's `<name>.package.nml`, E35): rule 3
/// keys claims on the declared name, so `evil.package.nml` declaring
/// `name = "demo"` beside the operator's `demo.package.nml` is a load
/// error naming both — closed-denied like every load error — never a
/// second `demo` that ambiguates the operator's.
///
/// `Err` is the WHY of the load failure, as the NML2088 sentence prints
/// it after `manifest failed to load: `. A refusal of the manifest's own
/// bytes is the reader's sentence (`too large: … — a package manifest is
/// read only up to …`, `absent`, `not UTF-8`) — the manifest is named
/// by the diagnostic's source, never wrapped as its own "declared
/// source". A declared source that cannot be read names the
/// source, its locator (`schemas[i].file`) in the manifest, the verdict
/// and the next step.
fn load_manifest(
    root: &WorkspaceRoot,
    key: &SourceKey,
    fs: &dyn LstatFs,
    read: ReadText<'_>,
    inputs: &mut LiveInputs,
    total: &mut usize,
    bounds: ByteBounds,
) -> Result<SchemaPackage, LoadFailure> {
    let text = read(InputKind::Manifest, &root.path_of(key)).map_err(LoadFailure::Input)?;
    match inputs.charge(text.len(), total, bounds) {
        Charge::Admitted => {}
        Charge::UnitSpent => return Err(LoadFailure::Budget(key.clone())),
        Charge::Backstop => return Err(LoadFailure::Backstop(key.clone())),
    }
    let dir = root.path_of(&key.dir());
    let key_dir = key.dir();
    let mut index = 0usize;
    let mut over_budget: Option<LoadFailure> = None;
    let package = SchemaPackage::from_parts(&text, |file| {
        // `from_parts` resolves `schemas` in declaration order, so the
        // call count is the entry's index.
        let locator = format!("declared source `{file}` (schemas[{index}].file in `{key}`)");
        index += 1;
        check_plain_file_name(file)
            .map_err(|why| format!("{locator} is unavailable: {why} — fix the `file` path"))?;
        let verdict = match fs.child(&dir, OsStr::new(file)) {
            Ok(Some(step)) => match step.kind {
                // The `lstat` above is per DECLARING manifest — the
                // link refusal is never cached — but the BYTES are read
                // once per `(directory, file)` in one discovery and
                // shared from here on.
                EntryKind::File => {
                    let source_key = key_dir.join(file);
                    if let Some(shared) = inputs.sources.get(&source_key) {
                        return Ok(Arc::clone(shared));
                    }
                    let text: Arc<str> = Arc::from(
                        read(InputKind::Source, &dir.join(file))
                            .map_err(|why| format!("{locator} is unavailable: {why}"))?,
                    );
                    match inputs.charge(text.len(), total, bounds) {
                        Charge::Admitted => {}
                        Charge::UnitSpent => {
                            over_budget = Some(LoadFailure::Budget(source_key));
                            return Err(format!("{locator}: live-input budget spent"));
                        }
                        Charge::Backstop => {
                            over_budget = Some(LoadFailure::Backstop(source_key));
                            return Err(format!("{locator}: live-input budget spent"));
                        }
                    }
                    inputs.sources.insert(source_key, Arc::clone(&text));
                    return Ok(text);
                }
                EntryKind::Symlink => {
                    "a symlink — a declared source is never read through a link; replace the \
                     link with the file itself"
                        .to_string()
                }
                EntryKind::Dir => "a directory, not a file — name a regular file".to_string(),
                EntryKind::Other => "not a regular file — name a regular file".to_string(),
            },
            Ok(None) => {
                "absent — create it beside the manifest, or fix the `file` path".to_string()
            }
            Err(e) => e.to_string(),
        };
        Err(format!("{locator} is unavailable: {verdict}"))
    })
    .map_err(|e| match e {
        // The closure's sentence is the whole story; `MissingSource`'s
        // own wrapper would name the source a second time.
        PackageError::MissingSource { detail, .. } => LoadFailure::Input(detail),
        // A manifest finding names the manifest it sits in — the key —
        // so the row reads `… at tenants/cu/tenant.package.nml:1:1:
        // missing required field …` wherever it is shown; it travels
        // typed, under its own code where it has one (NML2081).
        other => LoadFailure::Manifest {
            err: other,
            text: Arc::from(text.as_str()),
        },
    });
    if let Some(spent) = over_budget {
        return Err(spent);
    }
    let package = package?;
    let file_name = key.file_name();
    let stem = manifest_stem(file_name).unwrap_or(file_name);
    if stem != package.manifest.name {
        return Err(LoadFailure::Input(format!(
            "manifest file `{file_name}` declares name `{}` — a workspace manifest is \
             `<name>.package.nml` (expected `{}.package.nml`)",
            package.manifest.name, package.manifest.name
        )));
    }
    Ok(package)
}

/// Root-marker files, judged lazily and ONCE (R3′): a marker is live iff
/// no strictly shallower live workspace manifest claims its directory.
/// Only markers a LIVE package names are ever judged — a name declared by
/// an inert manifest is dead — so an inert manifest's markers produce no
/// NML2080 noise. Every judgement is well-founded whenever it is made:
/// the manifests that can claim a marker are strictly shallower than it,
/// and a manifest consults only markers at or above its own directory.
/// The walk is interleaved with the settlement, so `seen` GROWS between
/// calls and is passed per call rather than borrowed for the whole
/// settlement. The cached verdicts stay correct: a marker at
/// key depth *k* is judged against the manifests whose directory is a
/// strict ancestor of its own — key depth < *k* — and every one of
/// those is settled before round *k* lists anything.
#[derive(Default)]
struct Markers {
    /// Liveness per judged marker file.
    judged: BTreeMap<SourceKey, bool>,
}

impl Markers {
    fn is_live(
        &mut self,
        key: &SourceKey,
        live: &[ManifestClaim],
        inert: &mut Vec<Diagnostic>,
        probes: &mut usize,
    ) -> bool {
        if let Some(&verdict) = self.judged.get(key) {
            return verdict;
        }
        let verdict = match claiming_outer(live, key, probes) {
            Some(claimant) => {
                inert.push(diag::inert_input(key, "root marker", &claimant));
                false
            }
            None => true,
        };
        self.judged.insert(key.clone(), verdict);
        verdict
    }

    /// The live marker files named in `names`, shallowest first — those
    /// whose directory contains `within` when given (the ones that can
    /// anchor a manifest there, R4), else all of them.
    fn live_named(
        &mut self,
        seen: &[SourceKey],
        names: &[String],
        within: Option<&SourceKey>,
        live: &[ManifestClaim],
        inert: &mut Vec<Diagnostic>,
        probes: &mut usize,
    ) -> Vec<SourceKey> {
        if names.is_empty() {
            return Vec::new();
        }
        let candidates: Vec<SourceKey> = seen
            .iter()
            .filter(|key| {
                let name = key.file_name();
                names.iter().any(|n| n == name)
                    && !is_manifest_name(name)
                    && name != PROJECT_CONFIG_NAME
                    && within.is_none_or(|dir| key.dir().contains(dir))
            })
            .cloned()
            .collect();
        candidates
            .into_iter()
            .filter(|key| self.is_live(key, live, inert, probes))
            .collect()
    }
}

/// The key a unit's counters are kept under: the unit root, else the
/// root key for the root unit.
fn budget_key(unit: Option<&SourceKey>) -> SourceKey {
    unit.cloned().unwrap_or_else(SourceKey::root)
}

/// What a charge came to, when the universe survived it.
enum Spend {
    Admitted,
    /// This unit is spent: deny it ([`Walk::deny_unit`]).
    Unit(SourceKey),
}

/// One discovery in flight (A16 amendment): the walk's state, the
/// settlement's and the budgets, with every charge, purge and denial a
/// method — the unit rule is spelled ONCE, in [`Walk::charge`] and
/// [`Walk::deny_unit`], for the listing, the unit-root question and each
/// settlement step alike. [`discover`] documents the interleaving and the
/// attribution rules the methods implement.
struct Walk<'a> {
    root: &'a WorkspaceRoot,
    fs: &'a dyn PathFs,
    read: ReadText<'a>,
    budget: EntryBudget,
    out: Discovery,
    /// Every regular file the walk saw, by key, in settlement order —
    /// (depth, key): each round's listing is sorted before it is appended
    /// — the order `files` keeps. The leaf name is the key's own.
    seen: Vec<SourceKey>,
    live: LiveClaims,
    live_configs: Vec<ProjectConfigClaim>,
    markers: Markers,
    /// Live-input bytes over the WHOLE walk, every unit summed
    /// ([`ByteBounds::total`]).
    total_bytes: usize,
    /// The live-input byte bounds this walk charges against.
    bytes: ByteBounds,
    /// Live-input accounting PER BUDGET UNIT: the root unit's is the
    /// universe's, exactly as the entry bound's is.
    inputs: BTreeMap<SourceKey, LiveInputs>,
    /// The unit every LISTED directory was charged to (`None` is the
    /// root unit): a live input is charged to its directory's unit.
    dir_units: BTreeMap<SourceKey, Option<SourceKey>>,
    /// This round's listings: the files seen (settled once the round's
    /// directories are listed) and the directories to list next round,
    /// each with the budget unit it INHERITS from its parents.
    fresh: Vec<SourceKey>,
    next: Vec<(SourceKey, Option<SourceKey>)>,
}

impl<'a> Walk<'a> {
    fn new(
        root: &'a WorkspaceRoot,
        fs: &'a dyn PathFs,
        read: ReadText<'a>,
        budget: EntryBudget,
        bytes: ByteBounds,
        validators: Arc<ValidatorMemo>,
    ) -> Self {
        Self {
            root,
            fs,
            read,
            budget,
            bytes,
            out: Discovery::empty(root.clone(), validators),
            seen: Vec::new(),
            live: LiveClaims::default(),
            live_configs: Vec::new(),
            markers: Markers::default(),
            total_bytes: 0,
            inputs: BTreeMap::new(),
            dir_units: BTreeMap::new(),
            fresh: Vec::new(),
            next: Vec::new(),
        }
    }

    /// The whole discovery: the interleaved walk, then the external
    /// claims. A16: a universe the walk could not finish is denied in
    /// full — nothing binds, nothing is enumerated — and the truncation
    /// is the one note.
    fn run(mut self, extra: Vec<ExternalClaim>) -> Discovery {
        match self.walk(extra) {
            Ok(()) => {
                let mut out = self.out;
                out.claims = self.live.all;
                out.configs = self.live_configs;
                out.files = self.seen;
                sort_by_depth(&mut out.skipped);
                out
            }
            Err(truncation) => {
                let mut out = Discovery::empty(self.root.clone(), self.out.validators);
                out.truncated = Some(truncation);
                out
            }
        }
    }

    /// The walk and the liveness settlement, INTERLEAVED one key depth
    /// per round: list every directory at depth *d*, settle every input
    /// at key depth *d+1*, then list depth *d+1* (see [`discover`] for why
    /// the induction is sound and why the amendment needs it).
    fn walk(&mut self, extra: Vec<ExternalClaim>) -> Result<(), Truncation> {
        // (directory to list, the budget unit it INHERITS from its parents).
        let mut frontier: Vec<(SourceKey, Option<SourceKey>)> = vec![(SourceKey::root(), None)];
        while !frontier.is_empty() {
            for (dir, inherited) in frontier {
                self.list(dir, inherited)?;
            }
            // This depth's inputs — configs first (they depend on
            // strictly shallower manifests), then the manifests at that
            // depth: each is settled inert or live BEFORE it is read, and
            // a live one's anchor reads the live configs and own-markers
            // at or above its own directory. Keys are settled in the order
            // the round's listing sorted them into (depth, then key).
            self.fresh.sort();
            let round: Vec<SourceKey> = self.fresh.drain(..).collect();
            self.seen.extend(round.iter().cloned());
            self.settle_configs(&round)?;
            self.settle_manifests(&round)?;
            let mut next: Vec<(SourceKey, Option<SourceKey>)> = self.next.drain(..).collect();
            next.sort();
            frontier = next;
        }
        self.anchor_externals(extra)
    }

    /// Charge `n` entries or probes to `unit` (`None` is the root unit)
    /// at ONE chokepoint. `Err` is the whole universe denied — the
    /// backstop crossed, or the root unit spent (the operator's own
    /// content: its exhaustion is the whole universe, as it has always
    /// been) — at `dir`, the directory the charge belongs to.
    fn charge(
        &mut self,
        unit: Option<&SourceKey>,
        n: usize,
        dir: &SourceKey,
    ) -> Result<Spend, Truncation> {
        match self.budget.charge(unit, n) {
            Charge::Admitted => Ok(Spend::Admitted),
            Charge::Backstop => Err(Truncation::Entries { dir: dir.clone() }),
            Charge::UnitSpent => match unit {
                None => Err(Truncation::Entries { dir: dir.clone() }),
                Some(u) => Ok(Spend::Unit(u.clone())),
            },
        }
    }

    /// Whether `key` sits under a unit this walk already denied.
    fn denied(&self, key: &SourceKey) -> bool {
        self.out
            .truncated_units
            .iter()
            .any(|t| t.unit.contains(key))
    }

    /// The budget unit of the LISTED directory holding `key` (`None` is
    /// the root unit). A directory the walk never listed cannot hold a
    /// settled input; were one asked for, its charge would fall to the
    /// root unit — the universe's, fail-closed.
    fn unit_of(&self, key: &SourceKey) -> Option<SourceKey> {
        self.dir_units.get(&key.dir()).cloned().flatten()
    }

    /// Disown a spent budget unit (A16 amendment): nothing under `unit`
    /// is listed, settled, read or kept — every key under it is denied,
    /// and a partial listing would read as a whole one, the same reason
    /// a truncated walk leaves `files` empty. Dropped: the files seen in
    /// earlier rounds and this one, the directories still to list, the
    /// notes (an NML2080 about an input the denial already covers is
    /// noise on a file that validates under no binding; an NML2088 there
    /// would report a load the walk is disowning) and the LIVE claims and
    /// configs settled under it before the bound was reached (read,
    /// their bytes charged, then dropped: nothing outside the unit can
    /// depend on them, R5′). Nothing outside the unit moves, and the
    /// universe stays CLOSED: the claim that INDUCED the unit is above
    /// it and still counted.
    fn purge_unit(&mut self, unit: &SourceKey) {
        let under = |d: &Diagnostic| {
            d.source
                .as_deref()
                .and_then(SourceKey::checked)
                .is_some_and(|k| unit.contains(&k))
        };
        self.seen.retain(|k| !unit.contains(k));
        self.fresh.retain(|k| !unit.contains(k));
        self.next.retain(|(d, _)| !unit.contains(d));
        self.live
            .all
            .retain(|c| !c.manifest().is_some_and(|m| unit.contains(m)));
        self.live
            .operator
            .retain(|c| !c.manifest().is_some_and(|m| unit.contains(m)));
        self.live_configs.retain(|c| !unit.contains(&c.path));
        self.out.inert.retain(|d| !under(d));
        self.out.load_errors.retain(|d| !under(d));
        self.out.skipped.retain(|s| !unit.contains(&s.key));
    }

    /// Deny `unit`: purged, and recorded with where the walk stopped and
    /// which bound it met. The universe stays closed; only keys under
    /// the unit are denied.
    fn deny_unit(&mut self, unit: SourceKey, stop: SourceKey, why: UnitBound) {
        self.purge_unit(&unit);
        self.out
            .truncated_units
            .push(UnitTruncation { unit, stop, why });
    }

    /// List one directory of this round: settle its budget unit, charge
    /// the unit question's probes, list it under the unit's ledger, and
    /// queue what it holds — files for this round's settlement,
    /// subdirectories for the next round, and what the walk left out by
    /// policy for the closing row.
    fn list(&mut self, dir: SourceKey, inherited: Option<SourceKey>) -> Result<(), Truncation> {
        // Early termination, the half a `next.retain` cannot do: the
        // siblings a truncated unit already put in THIS round's
        // frontier are dropped unlisted too. Without it a tenant
        // spreading its entries over sibling directories at one
        // depth still pays for every one of them (measured:
        // 96 ms vs 52 ms on a 150,000-entry tenant).
        if self.denied(&dir) {
            return Ok(());
        }
        // Unit-root membership is decided the round the directory is
        // LISTED, never when it is enqueued: the manifest that makes
        // it one may be settled in between. Below a unit root the
        // answer is inherited — OUTERMOST wins, so a tenant whose
        // own manifest would mint units inside its subtree cannot
        // multiply its share of MAX_TOTAL_ENTRIES — and the glob DP
        // runs once per directory, not once per ancestor.
        // OPERATOR-LEVEL: a claim whose manifest sits in no content a
        // strictly shallower live claim reaches — under no unit
        // root, and in no gap of a shallower glob either. Only such
        // a claim can mint a unit root, inside a unit or at the root
        // unit: a tenant's live manifest in root-unit content (a
        // gap of the operator's glob) used to mint units inside its
        // own subtree, each with a full entry and byte budget.
        let mut probes = 0usize;
        let unit = match &inherited {
            // Inside a unit OUTERMOST still wins against every claim
            // a tenant could have committed inside it — but an
            // OPERATOR-LEVEL claim (above) may delegate at a NARROWER
            // boundary than a sibling catch-all does:
            // `tenants/**/*.flow.nml` beside
            // a root `**/*.model.nml` keeps `tenants/<x>` the unit,
            // not `tenants` (innermost among operator-level unit
            // roots, outermost once inside a unit).
            Some(u) => {
                if is_budget_unit(&self.live.operator, &dir, &mut probes) {
                    Some(dir.clone())
                } else {
                    Some(u.clone())
                }
            }
            // At the root unit, too, only an operator-level claim
            // mints one.
            None => is_budget_unit(&self.live.operator, &dir, &mut probes).then(|| dir.clone()),
        };
        // The unit question's probes are the parent's unit's work,
        // charged before the listing: a spent unit is not listed.
        if let Spend::Unit(u) = self.charge(inherited.as_ref(), probes, &dir)? {
            self.deny_unit(u, dir, UnitBound::Entries);
            return Ok(());
        }
        self.dir_units.insert(dir.clone(), unit.clone());
        let entries = match self.fs.list_dir(&self.root.path_of(&dir)) {
            Ok(entries) => entries,
            // An unlistable directory INSIDE a unit is that unit's:
            // nothing under it governs anything outside it,
            // so the unit is denied and nothing else moves; at the
            // root unit it is the whole universe, as it has always
            // been.
            Err(error) => match &unit {
                None => return Err(Truncation::Unreadable { dir, error }),
                Some(u) => {
                    let u = u.clone();
                    self.deny_unit(u, dir, UnitBound::Unreadable(error));
                    return Ok(());
                }
            },
        };
        let mut exhausted: Option<SourceKey> = None;
        {
            let mut ledger = self.budget.ledger(unit.as_ref());
            for (name, kind) in entries {
                match ledger.charge(1) {
                    Charge::Admitted => {}
                    Charge::Backstop => return Err(Truncation::Entries { dir }),
                    Charge::UnitSpent => match &unit {
                        // The root unit is the operator's own content:
                        // its exhaustion is the whole universe, as it
                        // has always been.
                        None => return Err(Truncation::Entries { dir }),
                        Some(u) => {
                            exhausted = Some(u.clone());
                            break;
                        }
                    },
                }
                // A name no key can carry (not UTF-8, or a `\` on unix) is
                // charged and never joined (`join` asserts plain names; a
                // tenant-committed `a\b.nml` aborted every debug-build
                // check under the root) — and REPORTED under the directory
                // holding it, with its kind, when it is content or may
                // hold some: a directory, a link, a `.nml`-named entry. An
                // unreported one let `nml check .` certify a tree with a
                // directory of content the walk never entered.
                let name = match listed(&name, kind) {
                    Some(Listed::Plain(plain)) => plain,
                    Some(Listed::Unkeyable { kind, name }) => {
                        self.out.skipped.push(Skipped {
                            key: dir.clone(),
                            why: Skip::UnkeyableName { kind, name },
                        });
                        continue;
                    }
                    None => continue,
                };
                match kind {
                    EntryKind::Dir => {
                        if walk_skips_dir(name) {
                            self.out.skipped.push(Skipped {
                                key: dir.join(name),
                                why: Skip::for_dir(name),
                            });
                            continue;
                        }
                        // An exact skip at the component bound, never a
                        // truncation (`child_dir`) — and a row: the
                        // directory is never listed, so content beneath
                        // it is unjudged, and a gate must say so.
                        match dir.child_dir(name) {
                            Some(sub) => self.next.push((sub, unit.clone())),
                            None => self.out.skipped.push(Skipped {
                                key: dir.join(name),
                                why: Skip::ComponentBound,
                            }),
                        }
                    }
                    EntryKind::File => {
                        if name.starts_with('.') && is_nml_name(name) {
                            self.out.skipped.push(Skipped {
                                key: dir.join(name),
                                why: Skip::DotFile,
                            });
                        }
                        self.fresh.push(dir.join(name));
                    }
                    // A symlinked entry is neither descended nor loaded:
                    // a link is never a resolution input — and it is
                    // REPORTED, whatever its name, since its kind (a
                    // directory of content?) is exactly what the walk
                    // never learns.
                    EntryKind::Symlink => self.out.skipped.push(Skipped {
                        key: dir.join(name),
                        why: Skip::Symlink,
                    }),
                    EntryKind::Other => {
                        if is_nml_name(name) {
                            self.out.skipped.push(Skipped {
                                key: dir.join(name),
                                why: Skip::Fifo,
                            });
                        }
                    }
                }
            }
        }
        if let Some(u) = exhausted {
            // Stopping here is where the amendment pays for itself:
            // the entries the tenant piled up are never listed.
            self.deny_unit(u, dir, UnitBound::Entries);
        }
        Ok(())
    }

    /// Settle this round's project configs: each is inert (NML2080) or
    /// live and read — its bytes charged like a manifest's: the entry
    /// bound admits ~32k of them per unit, and uncharged they were the
    /// one live input a tenant could make every invocation read and
    /// parse without limit — or unloadable (NML2088: a live config that
    /// cannot be read must not silently read as absent; its pins and
    /// autoAssociate are unknown).
    fn settle_configs(&mut self, round: &[SourceKey]) -> Result<(), Truncation> {
        for key in round
            .iter()
            .filter(|k| k.file_name() == PROJECT_CONFIG_NAME)
        {
            // Under a unit this round's settlement already spent:
            // skipped, never read (purged with the unit).
            if self.denied(key) {
                continue;
            }
            let mut probes = 0usize;
            let inert = claiming_outer(&self.live.all, key, &mut probes)
                .map(|claimant| diag::inert_input(key, "project config", &claimant));
            let unit = self.unit_of(key);
            if let Spend::Unit(u) = self.charge(unit.as_ref(), probes, &key.dir())? {
                self.deny_unit(u, key.clone(), UnitBound::Entries);
                continue;
            }
            if let Some(note) = inert {
                self.out.inert.push(note);
                continue;
            }
            match (self.read)(InputKind::ProjectConfig, &self.root.path_of(key)) {
                Ok(text) => {
                    match self
                        .inputs
                        .entry(budget_key(unit.as_ref()))
                        .or_default()
                        .charge(text.len(), &mut self.total_bytes, self.bytes)
                    {
                        Charge::Admitted => {}
                        // The universe-wide backstop: the whole
                        // universe, whatever the unit says.
                        Charge::Backstop => {
                            return Err(Truncation::TotalLiveInputBytes { key: key.clone() });
                        }
                        Charge::UnitSpent => match unit {
                            None => return Err(Truncation::LiveInputBytes { key: key.clone() }),
                            Some(u) => {
                                self.deny_unit(u, key.clone(), UnitBound::LiveInputBytes);
                                continue;
                            }
                        },
                    }
                    let file = nml_core::cst::parse_best_effort(&text);
                    self.live_configs.push(ProjectConfigClaim {
                        path: key.clone(),
                        config: ProjectConfig::from_file(&file),
                    });
                }
                Err(detail) => {
                    self.out.load_errors.push(diag::input_unloadable(
                        key,
                        "project config",
                        &detail,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Settle this round's manifests: each is inert (NML2080), unloadable
    /// (NML2088) or a live claim — settled operator-level or not ONCE,
    /// anchored at the nearest live config or own-marker directory at or
    /// above its own (R4), every probe charged to its unit before the
    /// claim stands.
    fn settle_manifests(&mut self, round: &[SourceKey]) -> Result<(), Truncation> {
        for key in round.iter().filter(|k| is_manifest_name(k.file_name())) {
            if self.denied(key) {
                continue;
            }
            let mut probes = 0usize;
            let inert = claiming_outer(&self.live.all, key, &mut probes)
                .map(|claimant| diag::inert_input(key, "package manifest", &claimant));
            let unit = self.unit_of(key);
            // Charged before the read, so a spent unit reads nothing.
            if let Spend::Unit(u) = self.charge(unit.as_ref(), probes, &key.dir())? {
                self.deny_unit(u, key.clone(), UnitBound::Entries);
                continue;
            }
            if let Some(note) = inert {
                self.out.inert.push(note);
                continue;
            }
            let package = match load_manifest(
                self.root,
                key,
                self.fs,
                self.read,
                self.inputs.entry(budget_key(unit.as_ref())).or_default(),
                &mut self.total_bytes,
                self.bytes,
            ) {
                Ok(package) => Arc::new(package),
                Err(LoadFailure::Input(why)) => {
                    self.out
                        .load_errors
                        .push(diag::input_unloadable(key, "manifest", &why));
                    continue;
                }
                Err(LoadFailure::Manifest { err, text }) => {
                    self.out
                        .load_errors
                        .push(diag::manifest_unloadable(key, &err));
                    self.out.failed_manifests.push((key.clone(), text));
                    continue;
                }
                // The universe-wide byte backstop is crossed: the whole
                // universe is denied, naming the input that
                // crossed it — the sixteen-tenant doctrine applied to
                // bytes.
                Err(LoadFailure::Backstop(spent)) => {
                    return Err(Truncation::TotalLiveInputBytes { key: spent });
                }
                // The unit's live-input budget is spent: the unit is
                // denied — at the root unit, the universe, as before.
                Err(LoadFailure::Budget(spent)) => match unit {
                    None => return Err(Truncation::LiveInputBytes { key: spent }),
                    Some(u) => {
                        self.deny_unit(u, spent, UnitBound::LiveInputBytes);
                        continue;
                    }
                },
            };
            // Operator-level or not, settled ONCE: does a strictly
            // shallower live claim reach a strict ancestor of dir(M)?
            // dir(M) itself never is reached — the inert check above
            // settled that — and every claim that could reach an
            // ancestor is settled already (R3′), so the verdict is final
            // the moment it is made.
            let mut ancestor = key.dir();
            let mut operator_level = true;
            let mut probes = 0usize;
            while ancestor.depth() > 0 {
                ancestor = ancestor.dir();
                if reaching_outer(&self.live.all, &ancestor, &mut probes).is_some() {
                    operator_level = false;
                    break;
                }
            }
            // R4: the nearest live config or OWN-marker directory at or
            // above dir(M), else dir(M).
            let dir = key.dir();
            let own_markers = self.markers.live_named(
                &self.seen,
                &package.manifest.root_markers,
                Some(&dir),
                &self.live.all,
                &mut self.out.inert,
                &mut probes,
            );
            // The settlement's remaining work — the operator-level
            // question and the marker scan — charged before the claim
            // stands: a unit spent by it keeps no claim from it.
            if let Spend::Unit(u) = self.charge(unit.as_ref(), probes, &dir)? {
                self.deny_unit(u, key.clone(), UnitBound::Entries);
                continue;
            }
            let anchor = self
                .live_configs
                .iter()
                .map(|c| c.path.dir())
                .chain(own_markers.iter().map(SourceKey::dir))
                .filter(|d| d.contains(&dir))
                .max_by_key(SourceKey::depth)
                .unwrap_or(dir);
            let claim =
                ManifestClaim::workspace(Arc::clone(&package), key.clone(), anchor, operator_level);
            if operator_level {
                self.live.operator.push(claim.clone());
            }
            self.live.all.push(claim);
        }
        Ok(())
    }

    /// External claims are MINTED here, anchored at their live, non-nested
    /// marker dirs (every manifest is settled by now, so any depth is
    /// well-founded); an external claim's marker scan is the root unit's
    /// work.
    fn anchor_externals(&mut self, extra: Vec<ExternalClaim>) -> Result<(), Truncation> {
        for claim in extra {
            let mut dirs: Vec<SourceKey> = Vec::new();
            let mut probes = 0usize;
            let mut marker_files = self.markers.live_named(
                &self.seen,
                &claim.package.manifest.root_markers,
                None,
                &self.live.all,
                &mut self.out.inert,
                &mut probes,
            );
            self.charge(None, probes, &SourceKey::root())?;
            marker_files.sort_by_key(SourceKey::depth);
            for key in &marker_files {
                let dir = key.dir();
                match dirs.iter().find(|d| d.is_strict_ancestor_of(&dir)) {
                    Some(outer) => self.out.inert.push(diag::nested_marker(
                        key,
                        claim.name(),
                        &outer.join(key.file_name()),
                    )),
                    None => dirs.push(dir),
                }
            }
            self.live.all.push(ManifestClaim::external(claim, dirs));
        }
        Ok(())
    }
}

/// Discover one root's universe. `extra` are the caller's store, injected
/// and builtin packages ([`ExternalClaim`]: the class by type, so a
/// caller cannot hand the walk a workspace claim); the walk mints their
/// claims at the live markers it settles.
///
/// The walk and the liveness settlement are INTERLEAVED, one key depth
/// per round (list every directory at depth *d*, settle every input at
/// key depth *d+1*, then list depth *d+1*). Sound by the induction the
/// settlement always ran on: an input at key depth *d+1* is inerted only
/// by a live manifest whose directory is a STRICT ancestor of its own —
/// a manifest at key depth ≤ *d*, settled in an earlier round — and a
/// live manifest's anchor consults only configs and own-markers at or
/// above its own directory, all of which the round that listed *d*
/// produced (R3′/R4). The interleaving is what the A16 amendment needs:
/// a directory's BUDGET UNIT is a function of the live claims above it,
/// so units must be known while the walk is still running.
///
/// A16 amendment. Every listing is charged to a budget unit: the
/// outermost unit root at or above the directory being listed (a
/// directory at some live claiming glob's unit boundary, the start of
/// its last run of wildcard directory segments — `tenants/<x>` for
/// `tenants/**/*.flow.nml`, `orgs/<o>/tenants/<t>` for
/// `orgs/*/tenants/**`), or the ROOT unit when there is
/// none. A unit past [`MAX_ENTRIES`] denies its own
/// subtree and nothing else; the root unit past [`MAX_ENTRIES`], or the
/// walk as a whole past [`MAX_TOTAL_ENTRIES`], truncates the universe
/// exactly as the single cumulative bound did before. Attribution is
/// OUTERMOST across the claims a tenant could commit and INNERMOST
/// among OPERATOR-LEVEL claims: a directory under a unit is that
/// unit's unless an operator-level claim makes it a narrower unit root
/// — a catch-all root binding beside the tenant binding no longer
/// widens the unit to the top-level directory — and at the root unit
/// only an operator-level claim mints one. A claim is
/// OPERATOR-LEVEL iff its manifest sits in no content a strictly
/// shallower live claim reaches: under no unit root, and in no gap of
/// a shallower glob either (`tenants/<x>/other/` under
/// `tenants/*/flows/**` is root-unit content, and a manifest there —
/// a tenant's — governs its subtree but mints nothing; minting there
/// once bought each such manifest a full budget of its own). The unit's
/// live-input bytes ([`MAX_LIVE_INPUT_BYTES`],
/// project configs included) and an unlistable directory under it are
/// charged to the unit the same way, and the bytes of the whole walk
/// are backstopped by [`MAX_TOTAL_LIVE_INPUT_BYTES`] as its entries are
/// by [`MAX_TOTAL_ENTRIES`].
///
/// DELTA against the two-phase walk this replaced: a WHOLE-universe
/// truncation can now happen after manifests strictly shallower than the
/// stop directory were already loaded. The result is byte-identical — a
/// truncated `Discovery` carries no claim, no config, no file and no
/// note — but "nothing was read" is now "nothing INERT was read": E28(4)
/// is untouched (liveness is still settled before any load), and the
/// reads that did happen were the operator's own live manifests, under
/// the same per-kind byte caps.
///
/// `validators` is the table the universe's bindings build their
/// validators into ([`ValidatorMemo`]): a front end that keeps universes
/// alive across rediscoveries (the editor) hands every discovery its
/// ONE table, so a rediscovered manifest with an unchanged hash costs no
/// second build; a one-shot run passes a fresh one.
pub fn discover(
    root: &WorkspaceRoot,
    fs: &dyn PathFs,
    read: ReadText<'_>,
    extra: Vec<ExternalClaim>,
    validators: Arc<ValidatorMemo>,
) -> Discovery {
    Walk::new(
        root,
        fs,
        read,
        EntryBudget::new(),
        ByteBounds::new(),
        validators,
    )
    .run(extra)
}

/// [`discover`] under SCALED entry bounds — the default test lane's
/// seam for the backstop pins the perf lane runs at full size
/// ([`MAX_ENTRIES`], [`MAX_TOTAL_ENTRIES`]); never a runtime knob.
#[cfg(test)]
pub(super) fn discover_scaled(
    root: &WorkspaceRoot,
    fs: &dyn PathFs,
    read: ReadText<'_>,
    extra: Vec<ExternalClaim>,
    unit_bound: usize,
    total_bound: usize,
) -> Discovery {
    Walk::new(
        root,
        fs,
        read,
        EntryBudget::bounded(unit_bound, total_bound),
        ByteBounds::new(),
        Arc::default(),
    )
    .run(extra)
}

/// [`discover`] under SCALED live-input byte bounds — the byte twin of
/// [`discover_scaled`], for the per-unit budget and the universe-wide
/// BACKSTOP the full-size bounds put out of a test's reach (a gigabyte
/// of live inputs); never a runtime knob.
#[cfg(test)]
pub(super) fn discover_byte_scaled(
    root: &WorkspaceRoot,
    fs: &dyn PathFs,
    read: ReadText<'_>,
    extra: Vec<ExternalClaim>,
    unit_bytes: usize,
    total_bytes: usize,
) -> Discovery {
    Walk::new(
        root,
        fs,
        read,
        EntryBudget::new(),
        ByteBounds {
            unit: unit_bytes,
            total: total_bytes,
        },
        Arc::default(),
    )
    .run(extra)
}

/// By depth then key — the order `files` keeps.
fn sort_by_depth(rows: &mut [Skipped]) {
    rows.sort_by(|a, b| (a.key.depth(), &a.key, &a.why).cmp(&(b.key.depth(), &b.key, &b.why)));
}

/// The checked file, resolved: its key, its governing binding, the
/// kernel's findings (NML2083 in a closed universe), and what the
/// verified leaf IS.
#[derive(Debug)]
pub struct Resolved<'a> {
    pub key: SourceKey,
    pub governing: Governing<'a>,
    pub findings: Vec<Diagnostic>,
    /// The verified leaf's `lstat` kind (E35, sec 1c): `None` when the
    /// leaf is absent or its parent chain could not be verified. A reader
    /// refuses anything but [`EntryKind::File`] (an open-trust
    /// [`EntryKind::Symlink`] is the developer's own and is followed by
    /// the path read) BEFORE any open — a FIFO named as the target is a
    /// typed error, never a blocked read.
    pub kind: Option<EntryKind>,
    /// What minting and verification learned about symlinks on the way
    /// to the key: under OPEN trust a followed link is REPORTED
    /// here — `Through(i)` indexes the first linked component of the
    /// root-relative spelling — so a front end can say so (`nml
    /// binding`'s `notes` row). A closed universe never reaches a
    /// `Through` verdict: its walk halts at the link (NML2083, in
    /// `findings`).
    pub via_symlink: SymlinkVerdict,
    /// The kernel's typed rejection when the path was refused (the one
    /// finding in `findings` is its rendering): a front end that wants
    /// the finding to name the path as its user typed it re-renders
    /// THIS through [`diag::path_finding_typed`] — never by editing the
    /// sentence.
    pub rejection: Option<PathError>,
    /// The universe's composition grant for the file ([`Grant`]): what
    /// `compose_file` is judged under — copied out of `governing` once,
    /// here, so no front end computes the governing binding a second
    /// time to learn it. A rejected path carries the universe's unbound
    /// form (closed, naming the root; or open).
    pub grant: Grant,
    /// The governing binding's validator, built (once per universe) by
    /// the kernel — `Some` exactly when the file is `Bound` and the
    /// binding's package composes: what every front end validates the
    /// file under, so no front end builds one for itself or judges a
    /// build failure in its own words. `None` for an unbound, ambiguous
    /// or rejected file — and for a bound file whose binding cannot
    /// build its validator, which `findings` then names (NML2091: the
    /// file validates under no binding at all). Its strictness is the
    /// binding's own — no front end tightens or loosens a bound file's
    /// verdict, so the CLI and the editor give one.
    pub validator: Option<Arc<SchemaValidator>>,
}

/// Mint the checked file's key under the universe's trust (closed ⇒ P4:
/// a symlinked component, ancestor or leaf, is NML2083 form 1; an
/// unverifiable spelling is form 2) and find its governing binding. A
/// rejected file is unbound with the rejection as its finding; an
/// ambiguously-claimed file keeps its `Ambiguous` governing and carries
/// the denial as an ERROR finding (rule 3), so no front end can read
/// the ambiguity as "the flags decide" and validate parse-only. Errors
/// are the kernel's non-diagnostic failures (the caller renders them
/// through `PathError`'s `Display`).
pub fn resolve_file<'a>(
    u: &Universe<'a>,
    path: &Path,
    fs: &dyn PathFs,
) -> Result<Resolved<'a>, PathError> {
    let trust = if u.is_closed() {
        Trust::Closed
    } else {
        Trust::Open
    };
    let rejected = |err: PathError| -> Result<Resolved<'a>, PathError> {
        let finding = diag::path_finding(&err).ok_or_else(|| err.clone())?;
        let key = match &err {
            PathError::SymlinkComponent { key, .. } | PathError::Unverifiable { key } => {
                Some(key.clone())
            }
            _ => None,
        };
        let Some(key) = key else {
            return Err(err);
        };
        Ok(Resolved {
            key,
            governing: Governing::Unbound,
            findings: vec![finding],
            kind: None,
            via_symlink: SymlinkVerdict::None,
            rejection: Some(err),
            grant: Grant::unbound(u),
            validator: None,
        })
    };
    // A key under a truncated budget unit is `Unbound` — and `Unbound`
    // alone is "the flags decide", i.e. parse-only with a green exit.
    // The denial is carried the way rule-3 ambiguity is: an
    // ERROR finding on the key, which every front end already counts,
    // so the verb exits 1 and the file is never read. It is NOT a
    // universe error: files outside the unit are unaffected.
    let denied_unit = |key: SourceKey, truncation: &'a UnitTruncation| Resolved {
        findings: vec![diag::unit_truncated(&key, truncation)],
        key,
        governing: Governing::Unbound,
        kind: None,
        via_symlink: SymlinkVerdict::None,
        rejection: None,
        // Nothing under a denied unit composes: the universe's unbound
        // form (closed, naming the root).
        grant: Grant::unbound(u),
        validator: None,
    };
    // The unit question is answered on the LEXICAL key, BEFORE any probe:
    // the walk refused the subtree, so nothing under it is `lstat`ed
    // here either. A target inside an unreadable unit used to reach the
    // OS's EACCES on its own path (`permission denied on a path
    // component`) as a bare error — exit 1 in `check`, 2 in `binding` —
    // with the unit's NML2089 row never spoken.
    if let Some(key) = SourceKey::under(u.root, path, fs) {
        if let Some(truncation) = u.truncated_unit(&key) {
            return Ok(denied_unit(key, truncation));
        }
    }
    let keyed = match SourceKey::mint(u.root, path, fs, trust) {
        Ok(keyed) => keyed,
        Err(err) => return rejected(err),
    };
    let (key, kind, via_symlink) = match keyed.verify(fs) {
        Ok(Some(verified)) => (verified.key, Some(verified.kind), verified.via_symlink),
        // Absent or unverifiable-in-open: the minted key names it.
        Ok(None) => (keyed.key, None, keyed.via_symlink),
        Err(err) => return rejected(err),
    };
    // An open universe's link can land a minted key under a denied unit
    // the spelling never named: the same answer.
    if let Some(truncation) = u.truncated_unit(&key) {
        return Ok(denied_unit(key, truncation));
    }
    let governing = governing(u, &key);
    let mut findings = match &governing {
        Governing::Ambiguous(claimants) => vec![diag::ambiguous_claim(&key, claimants)],
        Governing::Bound { .. } | Governing::Unbound => Vec::new(),
    };
    // The binding's validator, built ONCE per universe by the kernel's
    // table. A binding whose package cannot compose (a declared source
    // with a parse error, a duplicate definition, a cycle) is carried
    // the way rule-3 ambiguity is: an ERROR finding on the key (NML2091)
    // that every front end already counts, so the verb exits 1 and the
    // file validates under NOTHING — never under a registry or a
    // parse-only pass. It used to be each front end's own judgement:
    // the CLI refused the INVOCATION (exit 2, an uncoded sentence) while
    // the editor "fell back to basic validation" under a claim the CLI
    // never validates under.
    let validator = match &governing {
        Governing::Bound { claimant, .. } => {
            match u.validators.build(claimant.claim, claimant.binding) {
                Ok(validator) => Some(validator),
                Err(err) => {
                    findings.push(diag::validator_unbuildable(&key, claimant, &err));
                    None
                }
            }
        }
        Governing::Ambiguous(_) | Governing::Unbound => None,
    };
    let grant = Grant::of(u, &governing);
    Ok(Resolved {
        key,
        governing,
        findings,
        kind,
        via_symlink,
        rejection: None,
        grant,
        validator,
    })
}
