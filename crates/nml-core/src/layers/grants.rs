//! Grant lookup and composition-denial machinery (RFC 0019 §Authorization).

use crate::ast::BlockDecl;
use crate::diagnostic::{Diagnostic, Suggestion, codes};
use crate::span::Span;

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

impl LayerGrant {
    /// The grant's rules as every human surface spells them, in
    /// declaration order — `allowRefs[i] = "<glob>"`, `denyRefs[i] =
    /// "<glob>"`, `maxStackDepth = N` (no row when the grant sets no cap
    /// of its own): ONE spelling for `nml binding`'s rows and the editor's
    /// hover, so the two front ends cannot drift — each glob as the
    /// manifest reads it ([`crate::source_policy::string_literal`], the
    /// one string speller: a `{{` is never spelled raw).
    pub fn rules(&self) -> impl Iterator<Item = String> + '_ {
        let literal = crate::source_policy::string_literal;
        let allow = self
            .allow_refs
            .iter()
            .enumerate()
            .map(move |(i, g)| format!("allowRefs[{i}] = {}", literal(g)));
        let deny = self
            .deny_refs
            .iter()
            .enumerate()
            .map(move |(i, g)| format!("denyRefs[{i}] = {}", literal(g)));
        let cap = self
            .max_stack_depth
            .map(|depth| format!("maxStackDepth = {depth}"));
        allow.chain(deny).chain(cap)
    }
}

/// The `layers` object of the `--json` `binding` row and of the editor's
/// `nml/schemaInfo` — ONE spelling: `granted` alone when composition is
/// denied or the open context permits it, the grant's own fields when a
/// binding carries one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayersWire {
    pub granted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_refs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_refs: Option<Vec<String>>,
    /// `Some(None)` is a granted binding with no cap of its own — on the
    /// wire, `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_stack_depth: Option<Option<u32>>,
}

impl LayersWire {
    /// A binding's grant as the wire spells it: granted with its rules,
    /// or not (a binding without a `layers:` block).
    pub fn of(grant: Option<&LayerGrant>) -> Self {
        match grant {
            Some(g) => Self {
                granted: true,
                allow_refs: Some(g.allow_refs.clone()),
                deny_refs: Some(g.deny_refs.clone()),
                max_stack_depth: Some(g.max_stack_depth),
            },
            None => Self::context(false),
        }
    }

    /// No binding's grant: `granted` alone — true only for the open
    /// developer context.
    pub fn context(granted: bool) -> Self {
        Self {
            granted,
            allow_refs: None,
            deny_refs: None,
            max_stack_depth: None,
        }
    }
}

/// A package from OUTSIDE the workspace walk, by where it lives:
/// embedded in the binary by its embedder, published to the per-user
/// store, or built into nml — the resolution-precedence classes beyond
/// the workspace's own, in precedence order. Defined here, beside the
/// denial that names each one's remedy; the universe re-exports it, so
/// every front end speaks one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalClass {
    Injected,
    Store,
    Builtin,
}

/// Where a binding's manifest lives — what a denial's remedy can be. A
/// workspace manifest is the operator's own file: the remedy is a note
/// LOCATED at the binding (`at`, its name span) and the grant block as
/// an insertion there. A package from outside the walk is nobody's file
/// here: no note, no insertion — the sentence names where the manifest
/// lives and the change it needs. A state, not an `Option<Span>`: a
/// span without a file, or an external class with one, is
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestHome {
    Workspace { at: Span },
    External(ExternalClass),
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
    /// A binding governs the file and carries no `layers:` grant. `home`
    /// is where the binding's manifest lives ([`ManifestHome`]): the
    /// operator's workspace file, with the binding's name span — the
    /// denial's remedy is a LOCATED note and an insertion there — or a
    /// package from outside the walk, whose sentence names the change it
    /// needs; `package` is the binding's package name, the thing an
    /// operator republishes or rebuilds.
    NoGrant {
        binding: &'a str,
        manifest: &'a str,
        package: &'a str,
        home: ManifestHome,
    },
    /// Two or more manifests claim the file — denied, naming all claimants.
    Ambiguous { manifests: Vec<&'a str> },
    /// No binding governs the file; the context default applies (closed
    /// universe → denied, open developer context → permissive).
    Unbound { context: UnboundContext },
}

/// The context default an unbound file falls under (RFC 0019 E22). A
/// state, not a bool: NML2064's closed-universe form must name the
/// universe it refuses under, and a `bool` cannot carry that name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnboundContext {
    /// The open developer context — no manifest anywhere; composition is
    /// permitted.
    Open,
    /// A closed universe: at least one manifest was discovered and none
    /// claims the file. `claims` is the discovered-claim count the
    /// message reports. The universe's ROOT is a run-level fact every
    /// front end states once (the CLI's closing row and `binding` line,
    /// the editor's `nml/schemaInfo`), never a per-file sentence's: one
    /// sentence for both front ends, spelled the same wherever the run
    /// stands.
    Closed { claims: usize },
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
        GrantLookup::Unbound {
            context: UnboundContext::Open,
        }
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

/// The remedy beneath NML2064's no-grant form, as the structured
/// insertion every front end resolves
/// ([`SuggestionKind::Insert`](crate::diagnostic::SuggestionKind::Insert)):
/// the `layers:` block that goes under the binding, one `allowRefs`
/// entry per key a grant must admit — the referenced instances'
/// defining files ([`referenced_key_list`]: the checked file's OWN key
/// while every ref is same-file, the rule the located remedy note
/// states; RFC 0020's imports bring other keys). Written at zero
/// indentation in canonical steps ([`crate::cst::INDENT_UNIT`], the one
/// spelling the insertion engine decodes) — the resolver re-indents it to the
/// binding body's own, whatever the manifest's width — and each entry
/// spelled as an NML string literal through the language's one speller
/// ([`crate::source_policy::string_literal`]: the escape grammar the
/// lexer decodes, `\u{…}` for every character the source policy bans
/// raw, a `{{` re-escaped so the literal never reads as a template), so
/// a key's own characters are the snippet's content, never its
/// structure or its type: every raw newline here is the producer's.
pub(in crate::layers) fn no_grant_snippet(keys: &[&str]) -> String {
    let unit = crate::cst::INDENT_UNIT;
    let mut snippet = format!("layers:\n{unit}allowRefs:");
    for key in keys {
        snippet.push_str(&format!("\n{unit}{unit}- "));
        snippet.push_str(&crate::source_policy::string_literal(key));
    }
    snippet
}

/// NML2064's three message forms, from the grant state. `None` = permitted.
pub(in crate::layers) fn deny_diagnostic(
    lookup: &GrantLookup<'_>,
    id: InstanceId<'_>,
    block: &BlockDecl,
    refs: &[InstanceId<'_>],
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
        GrantLookup::Granted { .. }
        | GrantLookup::Unbound {
            context: UnboundContext::Open,
        } => None,
        GrantLookup::NoGrant {
            binding,
            manifest,
            package,
            home,
        } => {
            let keys = referenced_key_list(id, refs);
            // Why, and what to do now, by where the manifest lives (the
            // Core Test: what happened, why, the next action — the
            // sentence is the whole remedy where nothing can be located).
            let why = match home {
                ManifestHome::Workspace { .. } => {
                    "an operator change, not fixable from a content file".to_string()
                }
                ManifestHome::External(ExternalClass::Injected) => format!(
                    "the manifest is package `{package}`, embedded in this binary by its \
                     embedder and never edited in place: add the grant in the embedded \
                     package's source and rebuild"
                ),
                ManifestHome::External(ExternalClass::Store) => format!(
                    "the manifest is the store's current copy of package `{package}`, never \
                     edited in place: add the grant in the package's source and republish it"
                ),
                ManifestHome::External(ExternalClass::Builtin) => format!(
                    "the manifest is nml's own builtin package `{package}`, which grants no \
                     composition to the manifests it binds: write the manifest whole, \
                     without `uses`"
                ),
            };
            let denial = base(format!(
                "composition not permitted: binding '{binding}' ({manifest}) carries no \
                 `layers:` grant — {why}; run `nml binding {file}` to see the effective grant"
            ));
            // The remedy, LOCATED and STRUCTURED, for the operator's own
            // manifest: a note at the binding naming the exact key(s) an
            // `allowRefs` entry must admit — the referenced instances'
            // defining files, deduplicated (one file's own key while refs
            // are same-file; RFC 0020's imports bring others) — and the
            // block itself as an insertion under that binding, in that
            // manifest: the CLI prints it resolved beneath the row, the
            // editor offers it as a quick-fix on the manifest, the wire
            // carries the edit. A finding's message stays one sanitized
            // line by contract. A manifest from outside the walk is
            // nobody's to edit here: the sentence above is the remedy.
            Some(match home {
                ManifestHome::Workspace { at } => denial
                    .with_related_in(
                        *at,
                        format!(
                            "to permit it, give this binding a `layers:` grant whose \
                             `allowRefs` admits {}",
                            referenced_keys(&keys)
                        ),
                        Some(manifest.to_string()),
                    )
                    .with_suggestion(
                        Suggestion::insert(no_grant_snippet(&keys))
                            .at(*at)
                            .in_file(manifest.to_string()),
                    ),
                ManifestHome::External(_) => denial,
            })
        }
        GrantLookup::Ambiguous { manifests } => Some(base(format!(
            "composition not permitted: {} manifests claim this file ({}) — \
             an ambiguously-claimed file is denied; remove or narrow one \
             claim, then run `nml binding {file}`",
            manifests.len(),
            manifests.join(", ")
        ))),
        // The closed form names the UNIVERSE it refuses under (E22) by
        // its discovered-manifest count; the root itself is the run's
        // fact, stated once by each front end, so the sentence
        // is one text for the CLI and the editor wherever the run stands.
        GrantLookup::Unbound {
            context: UnboundContext::Closed { claims },
        } => Some(base(format!(
            "composition not permitted: no binding governs this file in the \
             closed universe ({claims} manifest(s) discovered) — add a `files` \
             glob that claims it (an operator change), then run `nml binding \
             {file}`"
        ))),
    }
}

/// The defining files of `refs`, deduplicated in order — the keys a
/// grant's `allowRefs` must admit for the clause to compose; the
/// declaring file's own when the refs are not yet resolved (a transitive
/// clause's site check runs first, and every ref is same-file until RFC
/// 0020's imports). ONE list feeds the located note and the `help:` block.
fn referenced_key_list<'a>(declaring: InstanceId<'a>, refs: &[InstanceId<'a>]) -> Vec<&'a str> {
    let mut keys: Vec<&str> = Vec::new();
    for r in refs {
        if !keys.contains(&r.source_path) {
            keys.push(r.source_path);
        }
    }
    if keys.is_empty() {
        keys.push(declaring.source_path);
    }
    keys
}

/// The keys quoted as a manifest lists them (`"a", "b"`) — the note's
/// spelling.
fn referenced_keys(keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("{k:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ───────────────────────────────────────────────────────── normalization ──
