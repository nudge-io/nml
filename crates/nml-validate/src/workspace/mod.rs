//! The shared binding-resolution core (RFC 0019 item 0): workspace roots,
//! canonical source keys and the path pipeline P1–P4 (`paths`) over an
//! injected filesystem oracle (`fs`); manifest claims, inertness and
//! the governing binding (`claims`); bounded discovery (`discover`);
//! the engine's grant provider (`grants`); and the typed→code table
//! (`diag`). The CLI and the LSP both call this — one matcher, one
//! selection rule, one path pipeline, so the editor and the CI gate can
//! never disagree about which binding governs a file.
//!
//! ONE surface: every submodule is private and this module re-exports
//! what a front end may name, so the layout below (which file holds the
//! walk, where a row is minted) can change without a caller noticing,
//! and no caller can reach a helper the facade did not publish. The
//! `nml limits` census keys a bound by its FILE (`declaredIn`), so the
//! private layout is still the one the wire names.
//!
//! Two layers. **Layer A** (`fs`, `paths`, `claims`, `grants`) has no
//! ambient authority: every observation goes through the injected
//! [`crate::fs::PathFs`], so a Layer-A function is a pure function of its
//! inputs and the oracle's answers — mockable, wasi-able, overlay-able.
//! **Layer B** (`discover`, `diag`) reads texts through a caller-supplied
//! reader, loads packages and speaks `Diagnostic`. A source ratchet keeps
//! ambient filesystem and environment access out of EVERY file here (the
//! oracle is the crate's own leaf, `crate::fs`, outside this module), and
//! a module-arrow ratchet (`tests/module_arrows.rs`)
//! keeps every dependency arrow between the crate's modules pinned.
//!
//! # Which binding governs this file?
//!
//! The whole question in four calls — a root, a discovery, the file, the
//! answer. `nml binding` and the editor's hover both end here.
//!
//! ```no_run
//! use std::path::Path;
//! use std::sync::Arc;
//! use nml_validate::fs::{MAX_SOURCE_BYTES, StdFs, read_beneath};
//! use nml_validate::workspace::{
//!     Governing, InputKind, ValidatorMemo, WorkspaceRoot, discover, read_input, resolve_file,
//! };
//!
//! let fs = StdFs;
//! // `explicit` is a `--root` the caller chose; `derive` walks up from a
//! // target to its `.git` fence when the caller chose none.
//! let root = WorkspaceRoot::explicit(Path::new("."), &fs)?;
//!
//! // Texts come from the CALLER: the editor answers from its open
//! // buffers first, a one-shot run from the disk — and the disk case is
//! // the kernel's one reader (`read_input`: capped per input kind, opened
//! // through the race-free chain), so no front end reads a file the
//! // walk classified through a link swapped in after the fact.
//! let read = |kind: InputKind, path: &Path| read_input(&root, kind, path);
//! let memo = Arc::new(ValidatorMemo::default());
//! let discovery = discover(&root, &fs, &read, Vec::new(), memo.clone());
//!
//! let universe = discovery.universe();
//! let file = Path::new("tenants/cu/member-lookup.flow.nml");
//! // A `PathError` here is a kernel refusal, not a diagnostic. It
//! // is a `std::error::Error`, so `?` carries it as-is.
//! let resolved = resolve_file(&universe, file, &fs)?;
//!
//! match &resolved.governing {
//!     Governing::Bound { claimant, .. } => println!("{}", claimant.binding.name),
//!     // Denied, never nearest-wins: no front end may fall back to
//!     // parse-only on either of these.
//!     Governing::Ambiguous(claimants) => println!("{} claimants", claimants.len()),
//!     Governing::Unbound => println!("no binding claims it"),
//! }
//! // Whatever the verdict, `resolved.findings` holds the rows to report.
//!
//! // Validate under the governing binding: `memo` builds its validator
//! // once per (content hash, binding), and every finding's `suggestions`
//! // carry the machine-applicable edits (a did-you-mean is one) that an
//! // editor offers as a quick fix and `nml fix` applies.
//! if let Governing::Bound { claimant, .. } = &resolved.governing {
//!     let validator = memo.build(claimant.claim, claimant.binding).map_err(|e| e.to_string())?;
//!     // A TARGET is read the way the front ends read one: anchored at the
//!     // root, component by component, capped — never a bare by-path open.
//!     let text = read_beneath(root.path(), &["tenants", "cu", "member-lookup.flow.nml"], MAX_SOURCE_BYTES, "a target")?;
//!     for finding in validator.validate(&nml_core::parse(&text)?) {
//!         println!("{:?}: {} ({} suggestion(s))", finding.severity, finding.message, finding.suggestions.len());
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod claims;
mod diag;
mod discover;
mod grants;
#[cfg(any(test, feature = "test-support"))]
mod mock;
mod notes;
mod paths;
mod vocabulary;

pub use crate::file_names::{
    PROJECT_CONFIG_NAME, SCHEMA_SOURCE_SUFFIXES, is_manifest_name, is_nml_name,
    is_schema_source_name, schema_source_stem,
};
pub use claims::{
    BindingStep, Built, ClaimClass, ClaimIdentity, ClaimOrigin, Claimant, Closure, ExternalClaim,
    ExternalClass, Governing, ManifestClaim, ProjectConfigClaim, UnitBound, UnitTruncation,
    Universe, UniverseState, ValidatorMemo, governing,
};
// The loader's tests pin the unit-gap rows; every product reader is a sibling of `diag`.
#[cfg(test)]
pub(crate) use diag::budget_unit_gaps;
pub use diag::{
    ambiguous_claim_summary, audit_incomplete, path_finding_typed, skipped, skipped_under,
};
pub use discover::{
    AuditBudget, Discovery, HiddenAudit, InputKind, MAX_AUDIT_EXAMPLES, MAX_ENTRIES,
    MAX_LIVE_INPUT_BYTES, MAX_TOTAL_ENTRIES, MAX_TOTAL_LIVE_INPUT_BYTES, ReadText, Resolved, Skip,
    Skipped, Truncation, audit_hidden, discover, input_cap, read_input, resolve_file,
    walk_skips_dir,
};
pub use grants::Grant;
#[cfg(any(test, feature = "test-support"))]
pub use mock::{MockFs, Probe, Spelling};
pub use paths::{
    Endpoint, Fence, Keyed, MAX_COMPONENTS, PathError, RootError, RootOrigin, Shadow, SourceKey,
    SymlinkVerdict, Trust, Verified, WorkspaceRoot,
};
pub use vocabulary::{SchemaUniverse, VocabularyMatch, VocabularyOutcome};

#[cfg(test)]
mod tests;
