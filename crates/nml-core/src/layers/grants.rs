//! Grant lookup and composition-denial machinery (RFC 0019 §Authorization).

use crate::ast::BlockDecl;
use crate::diagnostic::{Diagnostic, codes};

use super::instances::*;

/// A binding's composition grant. Attached to a validator binding; a
/// binding without one denies composition ([`GrantLookup::NoGrant`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerGrant {
    /// Target allowlist: globs over the referenced instance's defining
    /// file path; empty = deny all.
    pub allow_refs: Vec<String>,
    /// Deny wins over allow (NML2065 deny-veto, named by index).
    pub deny_refs: Vec<String>,
    /// Maximum distinct instances in one linearized stack, the declaring
    /// instance included (NML2066). `None` = no grant-level cap; the
    /// language hard cap still applies.
    pub max_stack_depth: Option<u32>,
}

/// The result of looking up the grant governing a file. A state, not an
/// `Option`: NML2064's three message forms need the binding and manifest
/// names, the ambiguous claimants, or the unclaiming root.
#[derive(Debug, Clone)]
pub enum GrantLookup<'a> {
    /// A binding governs the file and carries a grant.
    Granted {
        grant: &'a LayerGrant,
        binding: &'a str,
        manifest: &'a str,
    },
    /// A binding governs the file and carries no `layers:` grant.
    NoGrant { binding: &'a str, manifest: &'a str },
    /// Two or more manifests claim the file — denied, naming all claimants.
    Ambiguous { manifests: Vec<&'a str> },
    /// No binding governs the file; the context default applies (closed
    /// universe → denied, open developer context → permissive).
    Unbound { open_context: bool },
}

/// How a grant judged one referenced path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefDecision {
    Allowed,
    /// A `denyRefs` entry vetoed the path; the index names the rule
    /// (grant rules are unnamed strings — the index is the only stable
    /// referent, and `nml binding` prints the same indices).
    DenyVeto(usize),
    /// No `allowRefs` entry admits the path — the dominant denial mode
    /// ("empty allowlist means deny all"). No rule index exists.
    AllowMiss,
}

/// Answers the engine's two authorization questions. Matching (globs, path
/// canonicalization) is the provider's concern — nml-validate implements it
/// over its glob matcher and P1–P4 pipeline; tests implement it literally.
pub trait LayerGrantProvider {
    /// The grant state governing `source_path` (canonical workspace-relative).
    fn grant_for(&self, source_path: &str) -> GrantLookup<'_>;
    /// Evaluate one referenced defining path against a grant's rules.
    fn ref_decision(&self, grant: &LayerGrant, target_path: &str) -> RefDecision;
}

/// The open developer context: no manifest governs anything, composition is
/// permitted everywhere (RFC 0019's context default for a repo with no
/// binding). Closed universes get their provider from binding resolution.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenContext;

impl LayerGrantProvider for OpenContext {
    fn grant_for(&self, _source_path: &str) -> GrantLookup<'_> {
        GrantLookup::Unbound { open_context: true }
    }
    fn ref_decision(&self, _grant: &LayerGrant, _target_path: &str) -> RefDecision {
        RefDecision::Allowed
    }
}

// ─────────────────────────────────────────────────────── instance index ──

/// The governing grant's identity, for the denial family's contract
/// tail (RFC 0019, "recovery paths are part of the contract"): every
/// denial names the binding AND its manifest file, states plainly that
/// the change is an operator's, and ends by pointing at
/// `nml binding <file>`.
pub(in crate::layers) struct GrantRef<'a> {
    pub(in crate::layers) binding: &'a str,
    pub(in crate::layers) manifest: &'a str,
    /// The checked file — interpolated into the recovery pointer.
    pub(in crate::layers) file: &'a str,
}

/// Where a `uses` denial was raised — a named scope beats the earlier
/// `Option<Option<&str>>`, which a reader had to decode as
/// site / stack-anonymous / stack-with-entering-ref.
pub(in crate::layers) enum Denial<'a> {
    /// A declaring or transitive clause's own listed ref — the ref name
    /// IS the author's token, so it may be disclosed.
    Site,
    /// The root grant bounding a transitively-pulled layer the author
    /// never named. `entering` is the root clause's own listed ref that
    /// pulls it in (the author's token), when known.
    Stack { entering: Option<&'a str> },
}

pub(in crate::layers) fn ref_denial(
    decision: RefDecision,
    ref_name: &str,
    grant: &GrantRef<'_>,
    scope: Denial<'_>,
) -> Option<Diagnostic> {
    let GrantRef {
        binding,
        manifest,
        file,
    } = grant;
    // Stack-level denials name the ENTERING ref — the root clause's
    // listed ref whose stack pulls the denied layer in — so the author
    // knows which ref to remove (the denied layer itself may sit several
    // clauses away).
    let suffix = match scope {
        Denial::Stack {
            entering: Some(entering),
        } => format!(
            " (stack-level: the root grant bounds every composed layer; \
             '{entering}' in this clause pulls it in)"
        ),
        Denial::Stack { entering: None } => {
            " (stack-level: the root grant bounds every composed layer)".to_string()
        }
        Denial::Site => String::new(),
    };
    // The denial family's contract tail (RFC 0019): binding AND manifest
    // named, operator ownership stated, recovery pointer last.
    let tail = format!(" — an operator change, not fixable here; run `nml binding {file}`");
    let msg = match decision {
        RefDecision::Allowed => return None,
        RefDecision::DenyVeto(i) => format!(
            "`uses` ref '{ref_name}' denied by denyRefs[{i}] of binding \
             '{binding}' ({manifest}){suffix}{tail}"
        ),
        // Allow-miss discloses the BINDING, never the missed target: at
        // the site level the ref name is the author's own token, but a
        // stack-level denial reaches layers the author never named —
        // echoing a transitively-pulled layer's (author-chosen) instance
        // name would leak through the denial. The entering ref in the
        // suffix is the author's own clause token.
        RefDecision::AllowMiss => match scope {
            Denial::Site => format!(
                "`uses` ref '{ref_name}' denied: no allowRefs entry of \
                 binding '{binding}' ({manifest}) admits this layer{tail}"
            ),
            Denial::Stack { .. } => format!(
                "`uses` stack denied: no allowRefs entry of binding \
                 '{binding}' ({manifest}) admits a composed layer{suffix}{tail}"
            ),
        },
    };
    Some(Diagnostic::error(msg).with_code(codes::LAYER_REF_DENIED))
}

/// NML2064's three message forms, from the grant state. `None` = permitted.
pub(in crate::layers) fn deny_diagnostic(
    lookup: &GrantLookup<'_>,
    id: InstanceId<'_>,
    block: &BlockDecl,
) -> Option<Diagnostic> {
    let base = |msg: String| {
        Diagnostic::error(msg)
            .with_code(codes::COMPOSITION_DENIED)
            .with_span(block.name.span)
            .with_source(id.source_path.to_string())
    };
    // The recovery pointer names the CHECKED file — a literal `<file>`
    // placeholder when the emitter knows the path is a wall, not a
    // doorway.
    let file = id.source_path;
    match lookup {
        GrantLookup::Granted { .. } | GrantLookup::Unbound { open_context: true } => None,
        GrantLookup::NoGrant { binding, manifest } => Some(base(format!(
            "composition not permitted: binding '{binding}' ({manifest}) \
             carries no `layers:` grant — an operator change, not fixable \
             from a content file; run `nml binding {file}` to see the \
             effective grant"
        ))),
        GrantLookup::Ambiguous { manifests } => Some(base(format!(
            "composition not permitted: {} manifests claim this file ({}) — \
             an ambiguously-claimed file is denied; remove or narrow one \
             claim, then run `nml binding {file}`",
            manifests.len(),
            manifests.join(", ")
        ))),
        GrantLookup::Unbound {
            open_context: false,
        } => Some(base(format!(
            "composition not permitted: no binding governs this file in a \
             closed universe — add a `files` glob that claims it (an \
             operator change), then run `nml binding {file}`"
        ))),
    }
}

// ───────────────────────────────────────────────────────── normalization ──
