//! Stack discovery and C3 linearization (NML's reversed orientation), with the cycle, depth-cap and keyword denials.

use std::collections::{HashMap, HashSet};

use crate::ast::BlockDecl;
use crate::diagnostic::{Diagnostic, codes};
use crate::span::Span;

use super::grants::*;
use super::instances::*;
use super::*;

/// C3 merge over precedence-ordered lists (head = highest precedence).
/// Returns `None` when no consistent linearization exists (NML2077).
fn c3_merge<'a>(mut seqs: Vec<Vec<InstanceId<'a>>>) -> Option<Vec<InstanceId<'a>>> {
    let mut result = Vec::new();
    seqs.retain(|s| !s.is_empty());
    while !seqs.is_empty() {
        let head = seqs
            .iter()
            .map(|s| s[0])
            .find(|h| !seqs.iter().any(|s| s[1..].contains(h)))?;
        result.push(head);
        for s in &mut seqs {
            s.retain(|x| *x != head);
        }
        seqs.retain(|s| !s.is_empty());
    }
    Some(result)
}

/// Linearize the stack rooted at `declaring` with listed `refs`, in NML's
/// reversed orientation: precedence increases left to right, so the C3
/// merge runs over REVERSED ref lists and the result is read back-to-front
/// into bottom-up compose order. The declaring instance is the final (top)
/// element.
///
/// Returns bottom-up order (lowest first). Site authorization runs during
/// discovery (RFC 0019 step 1 is logical layering, not temporal phasing).
pub(in crate::layers) struct Linearizer<'a, 'p> {
    pub(in crate::layers) instances: &'a InstanceIndex<'a>,
    pub(in crate::layers) grants: &'p dyn LayerGrantProvider,
    pub(in crate::layers) declaring_keyword: &'a str,
    pub(in crate::layers) diags: Vec<Diagnostic>,
    /// Memoized precedence-ordered (head-first) linearizations.
    pub(in crate::layers) memo: HashMap<InstanceId<'a>, Option<Vec<InstanceId<'a>>>>,
    /// The live recursion path, in order — doubles as the cycle detector
    /// and lets NML2061 render the full cycle (`a -> b -> a`), matching
    /// the house cycle-diagnostic vocabulary.
    pub(in crate::layers) in_progress: Vec<InstanceId<'a>>,
    /// One-home guard for the discovery-depth NML2066: the guard fires
    /// once per frame otherwise (one chain, N over-cap instances, N
    /// identical-cause errors), and the clause-level depth report usually
    /// follows anyway.
    pub(in crate::layers) depth_reported: bool,
}

impl<'a, 'p> Linearizer<'a, 'p> {
    /// Precedence-ordered (head-first) linearization of one instance.
    pub(in crate::layers) fn linearize(
        &mut self,
        id: InstanceId<'a>,
    ) -> Option<Vec<InstanceId<'a>>> {
        if let Some(done) = self.memo.get(&id) {
            return done.clone();
        }
        // Discovery-time recursion bound: the post-linearization depth check
        // cannot protect the linearizer itself — a generated 10,000-link
        // chain must fail HERE, at 16 frames, not after recursing its full
        // length (RFC 0019: "a runaway generator cannot stack-bomb the
        // checker"). `in_progress` is exactly the live recursion path.
        if self.in_progress.len() as u32 >= MAX_STACK_DEPTH {
            if !self.depth_reported {
                self.depth_reported = true;
                let mut d = layer_bound_exceeded(LayerBound::Discovery { instance: id.name })
                    .with_source(id.source_path.to_string());
                // Anchor at the instance whose clause the recursion was
                // entering — a span-less diagnostic renders 0:0 in the
                // CLI and is DROPPED by the editor's span policy.
                if let Some(block) = self.instances.get(id) {
                    d = d.with_span(block.name.span);
                }
                self.diags.push(d);
            }
            return None;
        }
        if self.in_progress.contains(&id) {
            let start = self
                .in_progress
                .iter()
                .position(|x| *x == id)
                .expect("contains");
            // Canonicalize the rotation (smallest member name first): the
            // same cycle discovered from different roots must render — and
            // anchor — identically, so multi-root composes report ONE
            // finding through FindingKey dedup, not one per entry point.
            let cycle: Vec<InstanceId<'a>> = self.in_progress[start..].to_vec();
            let pivot = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, x)| x.name)
                .map(|(i, _)| i)
                .expect("cycle is non-empty");
            let mut path: Vec<&str> = cycle[pivot..]
                .iter()
                .chain(cycle[..pivot].iter())
                .map(|x| x.name)
                .collect();
            path.push(cycle[pivot].name);
            let anchor = cycle[pivot];
            let teach = if cycle.len() == 1 {
                format!(
                    "'{}' must not `uses` itself; remove the self-reference",
                    anchor.name
                )
            } else {
                "one of these is the base and must not `uses` the other(s); \
                 break the cycle"
                    .to_string()
            };
            let mut d = Diagnostic::error(format!(
                "`uses` reference cycle: {} — {teach}",
                path.join(" -> ")
            ))
            .with_code(codes::LAYER_CYCLE)
            .with_source(anchor.source_path.to_string());
            if let Some(block) = self.instances.get(anchor) {
                d = d.with_span(block.name.span);
            }
            self.diags.push(d);
            return None;
        }
        self.in_progress.push(id);
        let result = self.linearize_inner(id);
        self.in_progress.pop();
        self.memo.insert(id, result.clone());
        result
    }

    fn linearize_inner(&mut self, id: InstanceId<'a>) -> Option<Vec<InstanceId<'a>>> {
        let Some(block) = self.instances.get(id) else {
            // The one failure the resolve path could previously return
            // WITHOUT a diagnostic — a vanishing instance with no
            // explanation. Unreachable through `compose_file` (it
            // resolves refs first), but `resolve_layers` is the
            // documented embedder entry point and its contract is
            // "every diagnostic in one pass".
            self.diags.push(
                Diagnostic::error(format!("layer '{}' is not in the instance index", id.name))
                    .with_code(codes::UNRESOLVED_LAYER_REF)
                    .with_source(id.source_path.to_string()),
            );
            return None;
        };
        // Every layer must declare the same model keyword (NML2062).
        if block.keyword.name != self.declaring_keyword {
            self.diags.push(
                Diagnostic::error(format!(
                    "`uses` target '{}' is a `{}`, not a `{}` — layers \
                     compose only within one model keyword{}",
                    id.name,
                    block.keyword.name,
                    self.declaring_keyword,
                    // Did-you-mean over in-scope SAME-keyword instances
                    // (RFC's NML2062 row) — the author probably reached
                    // for a near-named layer, not a cross-keyword one.
                    crate::suggest::suggest(
                        id.name,
                        self.instances.names().filter(|n| {
                            *n != id.name
                                && self
                                    .instances
                                    .resolve_ref(n)
                                    .and_then(|t| self.instances.get(t))
                                    .is_some_and(|b| b.keyword.name == self.declaring_keyword)
                        }),
                    )
                    .map(|h| format!(" — did you mean '{h}'?"))
                    .unwrap_or_default()
                ))
                .with_code(codes::LAYER_KEYWORD_MISMATCH)
                .with_span(block.name.span)
                .with_source(id.source_path.to_string()),
            );
            return None;
        }
        // Site authorization: this clause's listed refs against the
        // authoring site's own grant.
        let refs = self.site_checked_refs(id, block)?;
        let merged = self.merge_listed(id, block.name.span, &refs)?;
        let mut lin = vec![id];
        lin.extend(merged);
        Some(lin)
    }

    /// The ONE listed-refs → precedence-ordered-merge implementation:
    /// order-preserving dedupe (a literal duplicate ref is redundant but
    /// legal), reversed-orientation C3 over each ref's own linearization,
    /// and the NML2077 emission. Shared by every transitive clause (via
    /// [`Self::linearize_inner`]) and the declaring clause (via
    /// [`resolve_layers`]) — the two can never linearize differently.
    pub(in crate::layers) fn merge_listed(
        &mut self,
        id: InstanceId<'a>,
        span: Span,
        refs: &[InstanceId<'a>],
    ) -> Option<Vec<InstanceId<'a>>> {
        let mut deduped: Vec<InstanceId<'a>> = Vec::new();
        for r in refs {
            if !deduped.contains(r) {
                deduped.push(*r);
            }
        }
        let mut parent_lins = Vec::new();
        for r in deduped.iter().rev() {
            parent_lins.push(self.linearize(*r)?);
        }
        let order_constraint: Vec<InstanceId<'a>> = deduped.iter().rev().copied().collect();
        parent_lins.push(order_constraint);
        // Breadth guard, BEFORE the merge: the C3 loop is superlinear in
        // the number of sequences, and the post-linearization depth check
        // runs only after that work is spent — a single wide clause
        // (thousands of listed refs over shared sub-layers) would buy
        // minutes of CPU from kilobytes of input. The merged result
        // contains every distinct discovered instance exactly once, so a
        // distinct-count over the inputs (+1 for the declaring instance)
        // that already exceeds the cap is guaranteed NML2066 — reject in
        // linear time and never start the merge.
        let mut distinct: HashSet<InstanceId<'a>> = HashSet::new();
        for seq in &parent_lins {
            distinct.extend(seq.iter().copied());
        }
        if distinct.len() as u32 + 1 > MAX_STACK_DEPTH {
            self.diags.push(
                layer_bound_exceeded(LayerBound::Language {
                    depth: distinct.len() as u32 + 1,
                })
                .with_span(span)
                .with_source(id.source_path.to_string()),
            );
            return None;
        }
        match c3_merge(parent_lins) {
            Some(merged) => Some(merged),
            None => {
                self.diags.push(
                    self.explain_inconsistency(id, &deduped)
                        .with_span(span)
                        .with_source(id.source_path.to_string()),
                );
                None
            }
        }
    }

    /// NML2077 with the RFC's teaching shape: name the contradicting pair
    /// and the forcing clause, not just "something contradicts". Two
    /// causes exist (both detectable from the memoized per-ref
    /// linearizations, all ≤ the 16-layer cap, so this is trivial work on
    /// an already-failing path):
    ///
    /// 1. A listed ref is a transitive base of an EARLIER-listed ref —
    ///    listing it after its own dependent would order it above it.
    ///    Carries a machine-applicable remove-the-ref suggestion.
    /// 2. Two listed refs' own stacks order a shared pair oppositely —
    ///    no listing order can satisfy both; the stacks must be aligned.
    ///
    /// Falls back to the generic either/or wording only when neither
    /// pattern is found (a multi-clause interaction pinned mid-merge).
    fn explain_inconsistency(&self, id: InstanceId<'a>, deduped: &[InstanceId<'a>]) -> Diagnostic {
        let block = self.instances.get(id);
        let ref_span =
            |n: &str| block.and_then(|b| b.uses.iter().find(|u| u.name == n).map(|u| u.span));
        for (i, a) in deduped.iter().enumerate() {
            let Some(Some(a_lin)) = self.memo.get(a) else {
                continue;
            };
            for b in &deduped[i + 1..] {
                if a_lin.iter().skip(1).any(|x| x == b) {
                    let mut d = Diagnostic::error(format!(
                        "no consistent linearization for '{}' — '{}' is \
                         already a transitive base of '{}', so listing it \
                         after '{}' would order it above its own \
                         dependent; remove the redundant ref or list it \
                         before '{}'",
                        id.name, b.name, a.name, a.name, a.name
                    ))
                    .with_code(codes::INCONSISTENT_LINEARIZATION);
                    // The deletion span swallows the preceding separator
                    // (`, b` not `b`) so applying the fix never leaves a
                    // dangling comma; the contradicting ref is listed
                    // after its dependent, so a predecessor always exists.
                    let sugg = block.and_then(|blk| {
                        // A duplicated listed name would need every
                        // occurrence removed — one span cannot express
                        // that, and a machine fix that doesn't fix stalls
                        // `nml fix`'s strict-improvement loop. Suggest
                        // only when the name occurs once.
                        if blk.uses.iter().filter(|u| u.name == b.name).count() != 1 {
                            return None;
                        }
                        let idx = blk.uses.iter().position(|u| u.name == b.name)?;
                        // The contradicting ref is listed after its own
                        // dependent, so it is never first: `deduped`
                        // preserves first occurrences and the pair loops
                        // iterate the redundant ref after the ref it
                        // contradicts. The resolver owns the bytes — the
                        // separator, the colon on a bodiless header, and
                        // every refusal.
                        if idx == 0 {
                            return None;
                        }
                        Some(blk.uses[idx].span)
                    });
                    if let Some(s) = sugg {
                        d = d.with_deletion(s);
                    }
                    if let Some(ab) = self.instances.get(*a) {
                        d = d.with_related_in(
                            ab.name.span,
                            format!("'{}' already composes '{}' here", a.name, b.name),
                            Some(a.source_path.to_string()),
                        );
                    }
                    return d;
                }
            }
        }
        for (i, a) in deduped.iter().enumerate() {
            let Some(Some(a_lin)) = self.memo.get(a) else {
                continue;
            };
            for b in &deduped[i + 1..] {
                let Some(Some(b_lin)) = self.memo.get(b) else {
                    continue;
                };
                for (xi, x) in a_lin.iter().enumerate() {
                    for y in &a_lin[xi + 1..] {
                        let (Some(bx), Some(by)) = (
                            b_lin.iter().position(|e| e == x),
                            b_lin.iter().position(|e| e == y),
                        ) else {
                            continue;
                        };
                        if bx > by {
                            let mut d = Diagnostic::error(format!(
                                "no consistent linearization for '{}' — \
                                 '{}' and '{}' order the shared pair '{}', \
                                 '{}' oppositely in their own stacks; \
                                 align them: order '{}' above '{}' in \
                                 '{}', or '{}' above '{}' in '{}' — or \
                                 drop one ref",
                                id.name,
                                a.name,
                                b.name,
                                x.name,
                                y.name,
                                x.name,
                                y.name,
                                b.name,
                                y.name,
                                x.name,
                                a.name,
                            ))
                            .with_code(codes::INCONSISTENT_LINEARIZATION);
                            if let Some(s) = ref_span(b.name) {
                                d = d.with_related(
                                    s,
                                    format!("'{}' orders '{}' below '{}'", b.name, y.name, x.name),
                                );
                            }
                            return d;
                        }
                    }
                }
            }
        }
        // Case 3: neither two-clause pattern holds — the pairwise orders
        // ROTATE across three or more clauses. A C3 failure guarantees a
        // cycle in the union "x above y" constraint graph (no valid head
        // = every candidate has an incoming edge), so name it: the old
        // generic fallback asserted the two patterns just ruled out,
        // misleading exactly the reader who reached it.
        let mut edges: HashMap<(&str, &str), &str> = HashMap::new();
        for r in deduped {
            if let Some(Some(lin)) = self.memo.get(r) {
                for (i, x) in lin.iter().enumerate() {
                    for y in &lin[i + 1..] {
                        edges.entry((x.name, y.name)).or_insert(r.name);
                    }
                }
            }
        }
        for (i, lo) in deduped.iter().enumerate() {
            for hi in &deduped[i + 1..] {
                // Listing order: later-listed sits above earlier-listed.
                edges.entry((hi.name, lo.name)).or_insert(id.name);
            }
        }
        if let Some(cycle) = find_order_cycle(&edges) {
            let steps: Vec<String> = cycle
                .windows(2)
                .map(|w| {
                    let src = edges.get(&(w[0], w[1])).copied().unwrap_or(id.name);
                    format!("'{}' above '{}' (per '{}')", w[0], w[1], src)
                })
                .collect();
            return Diagnostic::error(format!(
                "no consistent linearization for '{}' — the listed stacks' \
                 orders rotate: {}; no listing order can satisfy them all — \
                 drop one ref or align the contradicting stacks",
                id.name,
                steps.join(", ")
            ))
            .with_code(codes::INCONSISTENT_LINEARIZATION);
        }
        let listed: Vec<&str> = deduped.iter().map(|r| r.name).collect();
        inconsistent_linearization(id.name, &listed)
    }

    /// Resolve and site-authorize one clause's listed refs.
    fn site_checked_refs(
        &mut self,
        id: InstanceId<'a>,
        block: &'a BlockDecl,
    ) -> Option<Vec<InstanceId<'a>>> {
        if block.uses.is_empty() {
            return Some(Vec::new());
        }
        let site = self.grants.grant_for(id.source_path);
        if let Some(diag) = deny_diagnostic(&site, id, block) {
            self.diags.push(diag);
            return None;
        }
        let mut refs = Vec::new();
        let mut ok = true;
        for r in &block.uses {
            let Some(target) = self.instances.resolve_ref(&r.name) else {
                self.diags.push(
                    unresolved_ref(self.instances, id.name, self.declaring_keyword, &r.name)
                        .with_span(r.span)
                        .with_source(id.source_path.to_string()),
                );
                ok = false;
                continue;
            };
            if let GrantLookup::Granted {
                grant,
                binding,
                manifest,
            } = &site
            {
                let decision = self.grants.ref_decision(grant, target.source_path);
                let gref = GrantRef {
                    binding,
                    manifest,
                    file: id.source_path,
                };
                if let Some(d) = ref_denial(decision, &r.name, &gref, Denial::Site) {
                    self.diags
                        .push(d.with_span(r.span).with_source(id.source_path.to_string()));
                    ok = false;
                    continue;
                }
            }
            refs.push(target);
        }
        ok.then_some(refs)
    }
}

/// The one NML2059 wording owner: unresolved `uses` ref with a
/// did-you-mean over in-scope same-keyword instances (the declaring
/// instance itself excluded — suggesting a self-cycle helps no one).
pub(in crate::layers) fn unresolved_ref(
    instances: &InstanceIndex<'_>,
    declaring_name: &str,
    keyword: &str,
    ref_name: &str,
) -> Diagnostic {
    let hint = crate::suggest::suggest(
        ref_name,
        instances.names().filter(|n| {
            *n != declaring_name
                && instances
                    .resolve_ref(n)
                    .and_then(|t| instances.get(t))
                    .is_some_and(|b| b.keyword.name == keyword)
        }),
    );
    let mut msg = format!("`uses` ref '{ref_name}' does not resolve");
    if let Some(h) = hint {
        msg.push_str(&format!(" — did you mean '{h}'?"));
    }
    Diagnostic::error(msg).with_code(codes::UNRESOLVED_LAYER_REF)
}

/// NML2062's schema-definition form — one wording owner for the
/// composing path (`compose_file`) and the definition verb
/// (`check_uses_refs`), so the two can never describe the defect
/// differently.
pub(in crate::layers) fn schema_def_uses_denial(
    block: &BlockDecl,
    source_path: &str,
) -> Diagnostic {
    let d = Diagnostic::error(format!(
        "`uses` is an instance clause — a `{}` definition cannot \
         compose layers; delete the clause",
        block.keyword.name
    ))
    .with_code(codes::LAYER_KEYWORD_MISMATCH)
    .with_span(block.name.span)
    .with_source(source_path.to_string());
    // The promised fix (RFC 0019 plan): delete the clause — structural;
    // the resolver computes the bytes (the clause node with its leading
    // space, and the colon rule on a bodiless header). The primary span
    // stays the name.
    match block.uses_span {
        Some(span) => d.with_deletion(span),
        None => d,
    }
}

/// NML2077's generic fallback wording — reached only when
/// `explain_inconsistency` (the one emission site, shared by the
/// declaring clause and every transitive clause via `merge_listed`)
/// cannot pin the contradiction to a named pair.
fn inconsistent_linearization(name: &str, listed: &[&str]) -> Diagnostic {
    Diagnostic::error(format!(
        "no consistent linearization for '{name}' — the `uses` orders of \
         [{}] contradict: a listed layer is a transitive base of an \
         earlier-listed layer, or two listed layers' own stacks order a \
         shared pair oppositely; reorder or drop the contradicting ref",
        listed.join(", ")
    ))
    .with_code(codes::INCONSISTENT_LINEARIZATION)
}

/// One NML2065 emission for both denial modes, shared by every check site
/// (site-level, declaring-clause, stack-level) so the wording — and the
/// disclosure rules riding on it — cannot drift per site.
/// First cycle in a pairwise-order constraint graph, rendered as the node
/// path with the closing node repeated (`[x, y, z, x]`). Bounded tiny:
/// nodes ≤ the 16-layer cap.
fn find_order_cycle<'n>(edges: &HashMap<(&'n str, &'n str), &'n str>) -> Option<Vec<&'n str>> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (above, below) in edges.keys() {
        adj.entry(above).or_default().push(below);
    }
    // Deterministic teaching: the roots are sorted below, but the
    // adjacency lists are built from HashMap iteration — unsorted, the
    // SAME cycle renders with a run-dependent rotation (caught by the
    // RFC 0025 oracle's self-comparison on day one).
    for nexts in adj.values_mut() {
        nexts.sort_unstable();
    }
    fn dfs<'n>(
        node: &'n str,
        adj: &HashMap<&'n str, Vec<&'n str>>,
        path: &mut Vec<&'n str>,
        done: &mut HashSet<&'n str>,
    ) -> Option<Vec<&'n str>> {
        if let Some(start) = path.iter().position(|n| *n == node) {
            let mut cycle: Vec<&str> = path[start..].to_vec();
            cycle.push(node);
            return Some(cycle);
        }
        if done.contains(node) {
            return None;
        }
        path.push(node);
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                if let Some(c) = dfs(next, adj, path, done) {
                    return Some(c);
                }
            }
        }
        path.pop();
        done.insert(node);
        None
    }
    let mut done: HashSet<&str> = HashSet::new();
    let mut nodes: Vec<&str> = adj.keys().copied().collect();
    nodes.sort_unstable();
    for n in nodes {
        if let Some(c) = dfs(n, &adj, &mut Vec::new(), &mut done) {
            return Some(c);
        }
    }
    None
}

/// Which composition bound NML2066 fires for. One wording owner for the
/// code, like every other diagnostic in this module — its three forms
/// were previously open-coded at four sites with drifting phrasings.
pub(in crate::layers) enum LayerBound<'n> {
    /// Discovery-time: the recursion frame count hit the language cap
    /// before the full stack depth is known, so the instance is named
    /// instead of a number.
    Discovery { instance: &'n str },
    /// Composition-time: the distinct-instance count exceeds the cap.
    Language { depth: u32 },
    /// The grant's operator-set `maxStackDepth`.
    Grant { depth: u32, cap: u32 },
}

pub(in crate::layers) fn layer_bound_exceeded(bound: LayerBound<'_>) -> Diagnostic {
    let msg = match bound {
        LayerBound::Discovery { instance } => format!(
            "layer stack exceeds the language cap ({MAX_STACK_DEPTH}) at \
             '{instance}' — restructure the stack"
        ),
        LayerBound::Language { depth } => format!(
            "layer stack depth {depth} exceeds the language cap \
             ({MAX_STACK_DEPTH}) — restructure the stack"
        ),
        LayerBound::Grant { depth, cap } => format!(
            "layer stack depth {depth} exceeds the grant's maxStackDepth \
             = {cap} (an operator change)"
        ),
    };
    Diagnostic::error(msg).with_code(codes::LAYER_BOUND_EXCEEDED)
}
