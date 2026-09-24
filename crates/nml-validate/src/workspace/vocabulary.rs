//! Which directive vocabulary covers a schema source (RFC 0030, RFC 0019
//! §Editor surface): the kernel's one answer, asked by `nml check` and
//! `nml validate` for a checked `.model.nml` and by the editor for a buffer —
//! so the same file is judged under the same vocabulary by every front end.

use std::path::PathBuf;
use std::sync::Arc;

use nml_core::diagnostic::{Diagnostic, codes};
use nml_core::model::ModelDef;
use nml_core::span::Span;

use super::claims::{ClaimClass, ClaimOrigin, Governing, governing};
use super::discover::Discovery;
use super::paths::SourceKey;
use crate::directives::Vocabulary;
use crate::file_names::{is_nml_name, is_schema_source_name};
use crate::package::SchemaPackage;

/// The validation universe a covered `.model.nml` file loads against.
#[derive(Debug, Clone)]
pub enum SchemaUniverse {
    /// Workspace `[]schema` entries as PATHS (manifest-dir joins, in
    /// declaration order); an editor's assembly reads them buffer-first.
    Declared(Vec<PathBuf>),
    /// A store/in-binary package's hash-verified source snapshot.
    Snapshot(Arc<SchemaPackage>),
    /// No usable declaration — the workspace registry set.
    None,
}

/// The directive vocabulary governing a `.model.nml` file (RFC 0030).
#[derive(Debug, Clone)]
pub struct VocabularyMatch {
    pub vocabulary: Vocabulary,
    /// Root-rule coverage only: the file sits next to the covering
    /// WORKSPACE manifest without being in its `[]schema`.
    pub undeclared_sibling: bool,
    pub universe: SchemaUniverse,
}

impl VocabularyMatch {
    /// Every finding the covering vocabulary has about a schema source: the
    /// directive verdicts ([`Vocabulary::judge`]) and, for an undeclared
    /// sibling, the NML5003 note naming the remedy — the one call both front
    /// ends make.
    pub fn judge(&self, models: &[ModelDef], source: &str) -> Vec<Diagnostic> {
        let mut out = self.vocabulary.judge(models, source);
        if self.undeclared_sibling {
            out.push(
                Diagnostic::info(format!(
                    "not part of package '{}'; add a []schema entry to participate",
                    self.vocabulary.package_name()
                ))
                .with_code(codes::UNDECLARED_SIBLING)
                .with_span(Span::empty(0)),
            );
        }
        out
    }
}

/// The answer [`Discovery::vocabulary_for`] gives. `Undetermined` is the
/// truncated universe (A16): the walk could not enumerate the root, so
/// coverage is honestly unknown — never a silent opaque. `Ambiguous` is
/// two or more root-level coverers none of which declares the file:
/// judged under no vocabulary, and SAID (a second package in a root used
/// to switch every undeclared source from judged to accept-anything in
/// silence). Both carry a note ([`Self::note`]) every front end shows.
#[derive(Debug, Clone)]
pub enum VocabularyOutcome {
    Covered(VocabularyMatch),
    Opaque,
    Undetermined,
    /// The covering packages' names, sorted.
    Ambiguous {
        candidates: Vec<String>,
    },
}

impl VocabularyOutcome {
    /// The covering vocabulary, when there is one — `None` for an opaque
    /// file, for the undetermined (truncated) universe and for an ambiguous
    /// coverage alike: the one reading of "judge under it, or not at all"
    /// the verdict verbs share.
    pub fn covered(self) -> Option<VocabularyMatch> {
        match self {
            Self::Covered(m) => Some(m),
            Self::Opaque | Self::Undetermined | Self::Ambiguous { .. } => None,
        }
    }

    /// What a front end SAYS when a schema source is judged under no
    /// vocabulary for a reason the author can act on — the ONE sentence per
    /// shape, an info at the top of the file, as NML5003 is: the truncated
    /// universe (coverage unknown), or an ambiguous coverage naming every
    /// candidate package. `None` for a covered or a plainly opaque file —
    /// a schema author outside any package is never nagged.
    pub fn note(&self) -> Option<Diagnostic> {
        let message = match self {
            Self::Covered(_) | Self::Opaque => return None,
            Self::Undetermined => "package coverage undetermined (root exceeds the scan bound); \
                                  declare this file in the package's []schema to get \
                                  directive vocabulary"
                .to_string(),
            Self::Ambiguous { candidates } => format!(
                "package coverage ambiguous: {} packages could cover this schema source ({}) \
                 and none declares it — judged under no vocabulary (every directive \
                 accepted); declare it in one package's []schema",
                candidates.len(),
                candidates.join(", ")
            ),
        };
        Some(Diagnostic::info(message).with_span(Span::empty(0)))
    }
}

impl Discovery {
    /// The non-builtin claims — by name AND class, since a shadowed store
    /// copy of a workspace-defined name never binds — that bind at least one
    /// discovered `.nml` file: RFC 0030's per-root coverage question, from
    /// the kernel's own enumeration (no second walk, no cap of its own),
    /// computed once per discovery.
    pub(crate) fn coverers(&self) -> &[(String, ClaimClass)] {
        self.coverers.get_or_init(|| {
            let universe = self.universe();
            let mut out: Vec<(String, ClaimClass)> = Vec::new();
            for key in self.files.iter().filter(|k| is_nml_name(k.file_name())) {
                if let Governing::Bound { claimant, .. } = governing(&universe, key) {
                    let id = (claimant.claim.name().to_string(), claimant.claim.class());
                    if id.1 != ClaimClass::Builtin && !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
            out
        })
    }

    /// The directive vocabulary covering `key` (RFC 0030): (a) a live
    /// workspace manifest whose `[]schema` declares this exact file; else
    /// (b) the unique non-builtin claim that binds at least one file under
    /// the root and can govern this file's directory (a workspace manifest
    /// governs its own subtree only; two or more coverers are an ambiguity,
    /// answered as `Ambiguous` naming them — never a silent opaque) — for a
    /// file SPELLED as a schema source
    /// ([`is_schema_source_name`]); an instance file a binding claims is
    /// never judged under a vocabulary; else opaque. A truncated universe
    /// answers `Undetermined`.
    pub fn vocabulary_for(&self, key: &SourceKey) -> VocabularyOutcome {
        if self.truncated.is_some() {
            return VocabularyOutcome::Undetermined;
        }
        let path = self.root().path_of(key);
        let declared_paths =
            |claim: &super::claims::ManifestClaim, dir: &SourceKey| -> Vec<PathBuf> {
                let dir = self.root().path_of(dir);
                claim
                    .package
                    .manifest
                    .schemas
                    .iter()
                    .map(|entry| dir.join(&entry.file))
                    .collect()
            };
        let vocabulary = |claim: &super::claims::ManifestClaim| {
            Vocabulary::new(claim.name(), claim.package.manifest.directives.clone())
        };
        // (a) Declared source: the authoring path.
        for claim in self
            .claims
            .iter()
            .filter(|c| matches!(c.origin(), ClaimOrigin::Workspace { .. }))
        {
            let Some(manifest) = claim.manifest() else {
                continue;
            };
            let declared = declared_paths(claim, &manifest.dir());
            if declared.contains(&path) {
                return VocabularyOutcome::Covered(VocabularyMatch {
                    vocabulary: vocabulary(claim),
                    undeclared_sibling: false,
                    universe: SchemaUniverse::Declared(declared),
                });
            }
        }
        // (b) Root-level coverage — of a schema source only. Two or more
        // coverers ⇒ ambiguity, named.
        if !is_schema_source_name(key.file_name()) {
            return VocabularyOutcome::Opaque;
        }
        let mut covering: Vec<VocabularyMatch> = Vec::new();
        for claim in self
            .claims
            .iter()
            .filter(|c| c.class() != ClaimClass::Builtin)
        {
            if !self
                .coverers()
                .iter()
                .any(|(n, c)| n == claim.name() && *c == claim.class())
            {
                continue;
            }
            let universe = match claim.manifest() {
                Some(manifest) => {
                    if !manifest.dir().contains(key) {
                        continue;
                    }
                    SchemaUniverse::Declared(declared_paths(claim, &manifest.dir()))
                }
                None => SchemaUniverse::Snapshot(Arc::clone(&claim.package)),
            };
            let undeclared_sibling = claim.manifest().is_some_and(|m| m.dir() == key.dir());
            covering.push(VocabularyMatch {
                vocabulary: vocabulary(claim),
                undeclared_sibling,
                universe,
            });
        }
        if covering.len() > 1 {
            let mut candidates: Vec<String> = covering
                .iter()
                .map(|m| m.vocabulary.package_name().to_string())
                .collect();
            candidates.sort();
            return VocabularyOutcome::Ambiguous { candidates };
        }
        match covering.pop() {
            Some(m) => VocabularyOutcome::Covered(m),
            None => VocabularyOutcome::Opaque,
        }
    }
}
