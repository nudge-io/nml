//! The universe's word, as rows: what a [`Discovery`] states to every
//! front end — its own errors (NML2089, then NML2088), or, when it
//! stands, its unit-layout notes (NML2092); what bears on ONE key (the
//! inert inputs on its chain, NML2080); and the per-unit truncation rows
//! (NML2089 under `truncatedUnits`). One concern, one file, as the
//! vocabulary question has (`vocabulary.rs`); the walk that fills the
//! discovery stays in `discover.rs`.

use nml_core::diagnostic::Diagnostic;

use super::claims::ClaimOrigin;
use super::diag;
use super::discover::Discovery;
use super::paths::SourceKey;

impl Discovery {
    /// The universe's own ERRORS, bearing on every file under it: the
    /// truncation (NML2089), then every live input that failed to load
    /// (NML2088), in discovery order. A universe with one validates
    /// NOTHING — every front end counts these as errors (E28), never
    /// degrading to parse-only checking.
    pub fn universe_errors(&self) -> Vec<Diagnostic> {
        let mut errors: Vec<Diagnostic> = self.truncation_error().into_iter().collect();
        errors.extend(self.load_errors.iter().cloned());
        errors
    }

    /// The NML2080 notes on `key`'s ancestor chain: the inert inputs that
    /// would have changed this file's resolution had they been live —
    /// in discovery order.
    pub fn inert_notes_for(&self, key: &SourceKey) -> Vec<Diagnostic> {
        self.inert
            .iter()
            .filter(|d| {
                d.source
                    .as_deref()
                    .and_then(SourceKey::checked)
                    .is_some_and(|s| s.dir().contains(key))
            })
            .cloned()
            .collect()
    }

    /// The universe's word on EVERY key, stated ONCE per run by each
    /// front end: its errors (NML2089, then NML2088) — or, when it
    /// stands, its unit-layout notes (NML2092). A universe that cannot
    /// be trusted lints nothing, and that rule lives here rather than
    /// in a front end, so none can print a layout note beside an
    /// unloadable universe. What bears on ONE key — the inert inputs
    /// on its chain — is [`Self::inert_notes_for`]; the two scopes are
    /// two methods and never one list, so a front end cannot state a
    /// universe note per target by reading the wrong one (a flat list
    /// once let `binding` do exactly that). A front end appends what only it
    /// can say — the CLI its `--root` advice on the truncation row.
    pub fn universe_notes(&self) -> Vec<Diagnostic> {
        let errors = self.universe_errors();
        if errors.is_empty() {
            self.layout_notes()
        } else {
            errors
        }
    }

    /// The universe's unit-LAYOUT notes (NML2092): for every
    /// OPERATOR-LEVEL workspace manifest — the only claims that mint
    /// budget units — the binding globs whose inferred unit leaves
    /// delegated content in the root unit, each attributed to its
    /// manifest's key and spanned at the glob (the editor shows them on
    /// the manifest document, the CLI once per run). A tenant's live
    /// manifest inside the operator's content mints no unit, so its
    /// layout is never linted.
    pub(crate) fn layout_notes(&self) -> Vec<Diagnostic> {
        self.claims
            .iter()
            .filter_map(|c| match &c.origin {
                ClaimOrigin::Workspace {
                    manifest,
                    operator_level: true,
                    ..
                } => Some((manifest, c)),
                _ => None,
            })
            .flat_map(|(manifest, c)| {
                diag::budget_unit_gaps(&c.package.manifest)
                    .into_iter()
                    .map(|d| d.with_source(manifest.as_str().to_string()))
            })
            .collect()
    }

    /// One NML2089 row per budget unit the walk stopped inside, attributed
    /// to the unit — the rows a `summary` lists under `truncatedUnits`,
    /// as sentences: every file under such a unit is denied and absent
    /// from [`Self::files`].
    pub fn unit_errors(&self) -> Vec<Diagnostic> {
        self.unit_errors_under(&[SourceKey::root()])
    }

    /// The same rows, for the units at or under any of `dirs` — a gate
    /// over DIRECTORY arguments. A denied unit's files are purged from
    /// [`Self::files`], so no target ever carries its row: without this
    /// a directory run judged every sibling, printed nothing about the
    /// denied subtree and exited 0 — the silent outcome the walk's
    /// completeness rule forbids. One sentence with the named-file path
    /// (the sentence [`Self::unit_errors`] prints), one rule for both
    /// front ends.
    pub fn unit_errors_under(&self, dirs: &[SourceKey]) -> Vec<Diagnostic> {
        self.truncated_units
            .iter()
            .filter(|t| dirs.iter().any(|d| d.contains(&t.unit)))
            .map(|unit| diag::unit_truncated(&unit.unit, unit))
            .collect()
    }
}
