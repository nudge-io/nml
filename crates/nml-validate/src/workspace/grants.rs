//! The engine's grant provider over a resolved universe (RFC 0019 item 0,
//! step 0c; Layer A). [`Grant`] is the universe's composition verdict for
//! ONE file, OWNED: what its governing binding grants, or why nothing
//! does — copied out of the borrowed [`Governing`] once, where the file
//! is resolved (`Resolved::grant`), so the CLI composes under it straight
//! from the resolution and the editor keeps it past the universe borrow.
//! One type, both front ends: `nml check` and the editor deny or permit
//! composition identically (NML2064/NML2065 with one sentence). It
//! answers `LayerGrantProvider`'s two questions: the grant state, and
//! how a grant judges a referenced name — deny-first, byte-exact. The
//! post-allow verification and deny-first re-judge of a MINTED target
//! key (A11: "the key the grant judged" is "the key that names what is
//! read") lands with RFC 0020's import minting, its first caller.

use nml_core::layers::{
    GrantLookup, LayerGrant, LayerGrantProvider, LayersWire, ManifestHome, RefDecision,
    UnboundContext,
};

use super::claims::{ClaimOrigin, Governing, Universe};

/// The universe's composition grant for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// A binding governs the file and carries a `layers:` grant.
    Granted {
        grant: LayerGrant,
        binding: String,
        manifest: String,
    },
    /// A binding governs the file and carries no grant (NML2064's
    /// no-grant form names both). `home` is where the binding's manifest
    /// lives: a WORKSPACE file the operator can edit, with the binding's
    /// name span — the denial's remedy note and insertion land there —
    /// or a package from outside the walk (injected, store, builtin),
    /// nobody's to edit here, whose sentence names the change it needs;
    /// `package` the binding's package name.
    NoGrant {
        binding: String,
        manifest: String,
        package: String,
        home: ManifestHome,
    },
    /// Two or more manifests claim the file — denied, naming all.
    Ambiguous { manifests: Vec<String> },
    /// No binding governs the file: `closed` is the closed universe's
    /// discovered-claim count (NML2064's closed form names it; the root
    /// is the run's fact, stated once); `None` is the open
    /// developer context.
    Unbound { closed: Option<usize> },
}

impl Grant {
    /// The open developer context: no universe applies (a file outside
    /// every root), composition is permitted.
    pub fn open() -> Self {
        Self::Unbound { closed: None }
    }

    /// What an unbound key gets under `u`: the closed form naming the
    /// universe, or the open context.
    pub fn unbound(u: &Universe<'_>) -> Self {
        Self::Unbound {
            closed: u.is_closed().then(|| u.workspace_claims()),
        }
    }

    /// The verdict for a key whose governing binding under `u` is
    /// `governing` — the one place the binding's grant is read
    /// (`resolve_file` copies it into `Resolved::grant`).
    pub(crate) fn of(u: &Universe<'_>, governing: &Governing<'_>) -> Self {
        match governing {
            Governing::Bound { claimant, .. } => match &claimant.binding.layers {
                Some(grant) => Self::Granted {
                    grant: grant.clone(),
                    binding: claimant.binding.name.clone(),
                    manifest: claimant.claim.manifest_label.clone(),
                },
                None => Self::NoGrant {
                    binding: claimant.binding.name.clone(),
                    manifest: claimant.claim.manifest_label.clone(),
                    package: claimant.claim.name().to_string(),
                    home: match claimant.claim.origin() {
                        ClaimOrigin::Workspace { .. } => ManifestHome::Workspace {
                            at: claimant.binding.span,
                        },
                        ClaimOrigin::External { class, .. } => ManifestHome::External(*class),
                    },
                },
            },
            Governing::Ambiguous(claimants) => Self::Ambiguous {
                manifests: claimants
                    .iter()
                    .map(|c| c.claim.manifest_label.clone())
                    .collect(),
            },
            Governing::Unbound => Self::unbound(u),
        }
    }
}

impl Grant {
    /// The one sentence both front ends print where composition is denied
    /// and no grant can be shown — `nml binding`'s `layers` row under a
    /// grantless binding and under a closed universe that claims nothing,
    /// the editor's hover.
    pub const DENIED: &'static str = "none — composition denied (NML2064)";

    /// This verdict as the wire spells it ([`LayersWire`]): the `--json`
    /// `binding` row's `layers` object and the editor's `nml/schemaInfo`
    /// carry the same.
    pub fn wire(&self) -> LayersWire {
        match self {
            Self::Granted { grant, .. } => LayersWire::of(Some(grant)),
            Self::NoGrant { .. } | Self::Ambiguous { .. } => LayersWire::context(false),
            Self::Unbound { closed } => LayersWire::context(closed.is_none()),
        }
    }
}

impl LayerGrantProvider for Grant {
    /// The engine passes the path it composes under; a grant is the
    /// verdict for exactly one file and answers for it whatever name the
    /// engine spells (step 0f: the key is the name).
    fn grant_for(&self, _source_path: &str) -> GrantLookup<'_> {
        match self {
            Self::Granted {
                grant,
                binding,
                manifest,
            } => GrantLookup::Granted {
                grant,
                binding,
                manifest,
            },
            Self::NoGrant {
                binding,
                manifest,
                package,
                home,
            } => GrantLookup::NoGrant {
                binding,
                manifest,
                package,
                home: *home,
            },
            Self::Ambiguous { manifests } => GrantLookup::Ambiguous {
                manifests: manifests.iter().map(String::as_str).collect(),
            },
            Self::Unbound { closed } => GrantLookup::Unbound {
                context: match closed {
                    None => UnboundContext::Open,
                    Some(claims) => UnboundContext::Closed { claims: *claims },
                },
            },
        }
    }

    /// Deny wins over allow; byte-exact globs — no case folding, so a
    /// case-insensitive filesystem never widens a rule (P3).
    fn ref_decision(&self, grant: &LayerGrant, target_path: &str) -> RefDecision {
        decide_ref(grant, target_path)
    }
}

/// The ONE reference decision over a grant's rules — `denyRefs` vetoes
/// first (the index names the rule), then `allowRefs` must admit the
/// path ("empty allowlist means deny all").
fn decide_ref(grant: &LayerGrant, target_path: &str) -> RefDecision {
    if let Some(i) = grant
        .deny_refs
        .iter()
        .position(|g| crate::glob::glob_match(g, target_path))
    {
        return RefDecision::DenyVeto(i);
    }
    if grant
        .allow_refs
        .iter()
        .any(|g| crate::glob::glob_match(g, target_path))
    {
        RefDecision::Allowed
    } else {
        RefDecision::AllowMiss
    }
}
