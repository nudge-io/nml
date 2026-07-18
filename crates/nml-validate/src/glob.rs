//! Minimal path-glob matching for schema-package validator bindings
//! (RFC 0030).
//!
//! Grammar, pinned by the RFC: patterns are `/`-separated segments; a literal
//! segment matches itself; `*` matches any run of characters *within* one
//! segment; a segment consisting solely of `**` matches zero or more whole
//! segments. Matching is over `/`-normalized, root-relative paths (callers
//! normalize `\` on Windows before matching). Deliberately hand-rolled — no
//! `regex` dependency, no character classes, no `?`, no brace expansion:
//! binding patterns are authored by package publishers against fixed layout
//! conventions, not by end users, and every accepted form is testable.

/// Patterns beyond this segment count are rejected outright (matching
/// returns `false`). Binding patterns are publisher-authored layout
/// conventions a handful of segments deep; the cap exists so a hostile
/// manifest cannot feed the matcher pathological input (the LSP runs this
/// per keystroke on attacker-controlled workspaces).
pub const MAX_PATTERN_SEGMENTS: usize = 64;

/// Bound on the subsumption product walk's explored state-pair count. The
/// subset construction over a `**`+`*`-heavy pattern is exponential in the
/// worst case — correctness proofs say nothing about complexity, and this
/// runs per keystroke on attacker-supplied manifests. On exceed, `subsumes`
/// returns `false` ("incomparable"), which only suppresses an authoring
/// warning — always safe.
const MAX_SUBSUMES_STATES: usize = 10_000;

/// Match a `/`-normalized relative path against a binding pattern.
///
/// Iterative two-row DP over segments — worst case O(pattern × path), no
/// recursion, no backtracking blowup: the naive "try every split per `**`"
/// formulation is exponential in the number of `**` segments, which a
/// malicious workspace manifest could exploit to hang the server.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    // Runs of consecutive `**` collapse to one (identical semantics).
    let mut pat: Vec<&str> = Vec::new();
    for seg in pattern.split('/') {
        if seg == "**" && pat.last() == Some(&"**") {
            continue;
        }
        pat.push(seg);
    }
    if pat.len() > MAX_PATTERN_SEGMENTS {
        return false;
    }
    let segs: Vec<&str> = path.split('/').collect();

    // dp[j] = does pat[..i] match segs[..j]; rolled over pattern rows.
    let mut dp = vec![false; segs.len() + 1];
    dp[0] = true;
    for &p in &pat {
        let mut next = vec![false; segs.len() + 1];
        if p == "**" {
            // `**` matches zero or more whole segments: prefix-or over dp.
            let mut any = false;
            for j in 0..=segs.len() {
                any |= dp[j];
                next[j] = any;
            }
        } else {
            for j in 1..=segs.len() {
                next[j] = dp[j - 1] && match_segment(p, segs[j - 1]);
            }
        }
        dp = next;
    }
    dp[segs.len()]
}

/// `*`-wildcard match within a single segment (no `/` crossing by
/// construction: segments are already split).
fn match_segment(pattern: &str, segment: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let seg: Vec<char> = segment.chars().collect();
    // Classic iterative glob with single-star backtracking.
    let (mut p, mut s) = (0, 0);
    let (mut star, mut star_s) = (None, 0);
    while s < seg.len() {
        if p < pat.len() && (pat[p] == seg[s]) {
            p += 1;
            s += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            star_s = s;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            star_s += 1;
            s = star_s;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn literals_match_exactly() {
        assert!(glob_match("nudge.nml", "nudge.nml"));
        assert!(!glob_match("nudge.nml", "nudge.server.nml"));
        assert!(!glob_match("nudge.nml", "sub/nudge.nml"));
    }

    #[test]
    fn star_stays_within_a_segment() {
        assert!(glob_match("apps/*/app.nml", "apps/demo/app.nml"));
        assert!(!glob_match("apps/*/app.nml", "apps/a/b/app.nml"));
        assert!(glob_match("*.package.nml", "nudge.package.nml"));
        // The RFC's load-bearing case: a bare `package.nml` has nothing
        // before the first dot-segment boundary the pattern requires.
        assert!(!glob_match("*.package.nml", "package.nml"));
        assert!(glob_match("nudge.*.nml", "nudge.server.nml"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("**/app.nml", "app.nml"));
        assert!(glob_match("**/app.nml", "a/b/c/app.nml"));
        assert!(glob_match("apps/**/app.nml", "apps/x/y/app.nml"));
        assert!(glob_match("apps/**/app.nml", "apps/app.nml"));
        assert!(!glob_match("apps/**/app.nml", "libs/x/app.nml"));
    }

    #[test]
    fn empty_star_runs_are_fine() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b", "ab"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b", "acx"));
    }

    /// Hostile patterns terminate fast: many `**` segments (the exponential-
    /// backtracking shape) collapse/DP to linear work, and absurdly long
    /// patterns are rejected by the segment cap instead of recursing.
    #[test]
    fn pathological_patterns_are_bounded() {
        let star_bomb = format!("{}/x", vec!["**"; 40].join("/"));
        let path = format!("{}/y", vec!["a"; 60].join("/"));
        let start = std::time::Instant::now();
        assert!(!glob_match(&star_bomb, &path));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "star bomb must not blow up: {:?}",
            start.elapsed()
        );
        let long_pattern = vec!["a"; 100_000].join("/");
        assert!(
            !glob_match(&long_pattern, "a/b"),
            "over-cap patterns are rejected"
        );
        // Interleaved `**`s still match correctly post-collapse.
        assert!(glob_match("**/a/**/b", "x/a/y/z/b"));
        assert!(glob_match("**/**/a", "a"));
    }
}

// ── Subsumption (RFC 0030 shadowed-binding warning) ──────────────────────────
//
// `subsumes(a, b)` decides L(b) ⊆ L(a) exactly, by the textbook construction:
// each glob translates to a character-level NFA (mirroring `glob_match`'s
// segment semantics), `a` is determinized over a byte-class alphabet and
// complemented, and a product walk with `b`'s NFA searches for a witness in
// L(b) ∖ L(a). Correct by construction — no bespoke simulation lemma — and
// exhaustively differential-tested against `glob_match` below.

/// Byte classes: the distinct literal bytes of both patterns, `/`, and one
/// "other" class. Every byte in any path falls into exactly one class, and
/// pattern transitions are constant within a class, so the DFA alphabet is
/// ~10 symbols regardless of path content.
struct Classes {
    /// Distinct non-`/` literal bytes; index = class id. `/` gets id
    /// `bytes.len()`, OTHER gets `bytes.len() + 1`.
    bytes: Vec<u8>,
}

impl Classes {
    fn build(patterns: [&str; 2]) -> Self {
        let mut bytes: Vec<u8> = Vec::new();
        for p in patterns {
            for &b in p.as_bytes() {
                if b != b'/' && b != b'*' && !bytes.contains(&b) {
                    bytes.push(b);
                }
            }
        }
        Self { bytes }
    }
    fn count(&self) -> usize {
        self.bytes.len() + 2
    }
    fn slash(&self) -> usize {
        self.bytes.len()
    }
    fn of_literal(&self, b: u8) -> usize {
        self.bytes
            .iter()
            .position(|&x| x == b)
            .expect("literal byte registered")
    }
}

/// NFA transition labels: one class, any non-`/` class, or epsilon (stored
/// separately). States are indices; `accept` is a single final state.
struct Nfa {
    /// Per state: (class id, target) edges.
    edges: Vec<Vec<(usize, usize)>>,
    /// Per state: targets reachable on any non-`/` class.
    non_slash: Vec<Vec<usize>>,
    eps: Vec<Vec<usize>>,
    start: usize,
    accept: usize,
}

impl Nfa {
    fn new() -> Self {
        Self {
            edges: Vec::new(),
            non_slash: Vec::new(),
            eps: Vec::new(),
            start: 0,
            accept: 0,
        }
    }
    fn state(&mut self) -> usize {
        self.edges.push(Vec::new());
        self.non_slash.push(Vec::new());
        self.eps.push(Vec::new());
        self.edges.len() - 1
    }

    /// Build the NFA for a pattern, mirroring `glob_match` exactly. Frame:
    /// `B_i` = boundary before atom i (a path segment is about to be read);
    /// `E_i` = end of the segment content atom i consumed. A normal atom
    /// chains chars from `B_i` to `E_i` (`*` = non-`/` loop) and exits
    /// `E_i --'/'--> B_{i+1}`. A `**` atom adds `B_i --ε--> B_{i+1}` (zero
    /// segments) and a content loop `B_i --[^/]*--> E_i`, `E_i --'/'--> B_i`
    /// (more segments), `E_i --'/'--> B_{i+1}`. Acceptance: any `E_i` whose
    /// following atoms are all `**` (they may all consume zero segments) —
    /// one uniform rule instead of trailing-separator special cases.
    fn build(pattern: &str, classes: &Classes) -> Self {
        let mut nfa = Self::new();
        // Collapse `**` runs, mirroring glob_match.
        let mut atoms: Vec<&str> = Vec::new();
        for seg in pattern.split('/') {
            if seg == "**" && atoms.last() == Some(&"**") {
                continue;
            }
            atoms.push(seg);
        }
        let k = atoms.len();
        let boundaries: Vec<usize> = (0..=k).map(|_| nfa.state()).collect();
        nfa.start = boundaries[0];
        let accept = nfa.state();
        nfa.accept = accept;

        for (i, atom) in atoms.iter().enumerate() {
            let b = boundaries[i];
            // Does every atom after i consume zero segments in some run?
            let tail_all_doublestar = atoms[i + 1..].iter().all(|a| *a == "**");
            let e = if *atom == "**" {
                nfa.eps[b].push(boundaries[i + 1]);
                let c = nfa.state();
                nfa.eps[b].push(c);
                nfa.non_slash[c].push(c);
                let e = nfa.state();
                nfa.eps[c].push(e);
                let slash = classes.slash();
                nfa.edges[e].push((slash, b));
                if i + 1 < k {
                    nfa.edges[e].push((slash, boundaries[i + 1]));
                }
                e
            } else {
                let mut cur = b;
                for &byte in atom.as_bytes() {
                    if byte == b'*' {
                        let s2 = nfa.state();
                        nfa.eps[cur].push(s2);
                        nfa.non_slash[s2].push(s2);
                        cur = s2;
                    } else {
                        let s2 = nfa.state();
                        nfa.edges[cur].push((classes.of_literal(byte), s2));
                        cur = s2;
                    }
                }
                if i + 1 < k {
                    let s2 = boundaries[i + 1];
                    nfa.edges[cur].push((classes.slash(), s2));
                }
                cur
            };
            if tail_all_doublestar {
                nfa.eps[e].push(accept);
            }
        }
        // Edge case: an all-`**` pattern must also accept via its own E
        // (handled by the loop) — and a zero-atom pattern cannot occur
        // (split always yields at least one atom).
        nfa
    }

    fn eps_closure(&self, set: &mut [bool]) {
        let mut stack: Vec<usize> = (0..set.len()).filter(|&i| set[i]).collect();
        while let Some(s) = stack.pop() {
            for &t in &self.eps[s] {
                if !set[t] {
                    set[t] = true;
                    stack.push(t);
                }
            }
        }
    }

    fn step(&self, set: &[bool], class: usize, slash: usize) -> Vec<bool> {
        let mut next = vec![false; set.len()];
        for (s, &on) in set.iter().enumerate() {
            if !on {
                continue;
            }
            for &(c, t) in &self.edges[s] {
                if c == class {
                    next[t] = true;
                }
            }
            if class != slash {
                for &t in &self.non_slash[s] {
                    next[t] = true;
                }
            }
        }
        self.eps_closure(&mut next);
        next
    }
}

/// Does `a` match every path `b` matches? Exact for this glob grammar.
/// Over-cap patterns (rejected by `glob_match`) subsume nothing and are
/// subsumed by anything that could match nothing — callers only pass
/// meta-validated patterns, so treat them as incomparable (false).
pub fn subsumes(a: &str, b: &str) -> bool {
    if a.split('/').count() > MAX_PATTERN_SEGMENTS || b.split('/').count() > MAX_PATTERN_SEGMENTS {
        return false;
    }
    let classes = Classes::build([a, b]);
    let na = Nfa::build(a, &classes);
    let nb = Nfa::build(b, &classes);
    let slash = classes.slash();

    // Product walk over (eps-closed B set, eps-closed A set), searching for
    // a reachable configuration where B accepts and A does not. The A-side
    // subset acts as its determinized (complete) DFA state.
    let mut sa = vec![false; na.edges.len()];
    sa[na.start] = true;
    na.eps_closure(&mut sa);
    let mut sb = vec![false; nb.edges.len()];
    sb[nb.start] = true;
    nb.eps_closure(&mut sb);

    let mut seen = std::collections::HashSet::new();
    let mut queue = vec![(sb, sa)];
    while let Some((cb, ca)) = queue.pop() {
        if cb[nb.accept] && !ca[na.accept] {
            return false;
        }
        if seen.len() >= MAX_SUBSUMES_STATES {
            // Complexity bound, not a correctness statement: give up on
            // comparing pathological patterns rather than hang the server.
            return false;
        }
        if !seen.insert((cb.clone(), ca.clone())) {
            continue;
        }
        for class in 0..classes.count() {
            let nb_next = nb.step(&cb, class, slash);
            if !nb_next.iter().any(|&x| x) {
                continue; // no B path — irrelevant to inclusion
            }
            let na_next = na.step(&ca, class, slash);
            queue.push((nb_next, na_next));
        }
    }
    true
}

#[cfg(test)]
mod subsumption_tests {
    use super::{glob_match, subsumes};

    #[test]
    fn known_relations() {
        assert!(subsumes("**/app.nml", "apps/*/app.nml"));
        assert!(subsumes("*", "a*b"));
        assert!(subsumes("a/**/b", "a/x/**/b"));
        assert!(subsumes("*.nml", "*.package.nml"));
        assert!(!subsumes("apps/*/app.nml", "**/app.nml"));
        assert!(!subsumes("a*", "*a"));
        assert!(!subsumes("a/b", "a/*"));
        assert!(subsumes("**", "a/**/b"));
    }

    /// The subsumption walk is complexity-bounded: a `**` + `*`-run pattern
    /// that explodes the subset construction terminates fast by giving up
    /// (returns false — a suppressed warning, never a hang). This is the
    /// `subsumes` analog of `pathological_patterns_are_bounded`.
    #[test]
    fn subsumption_star_bomb_is_bounded() {
        let bomb = format!("**/{}", vec!["*"; 30].join("/"));
        let victim = format!("**/{}/x", vec!["*"; 29].join("/"));
        let start = std::time::Instant::now();
        let _ = subsumes(&bomb, &victim);
        let _ = subsumes(&victim, &bomb);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "subsumption must be complexity-bounded: {:?}",
            start.elapsed()
        );
    }

    /// Exhaustive differential proof over a bounded space: for every pattern
    /// pair, `subsumes` agrees with brute-force checking of every path. The
    /// bounded space covers every structural feature (empty segments,
    /// mid-segment stars, `**` at each position), so a defect in either the
    /// automata or the matcher shows up as a disagreement.
    #[test]
    fn differential_against_brute_force() {
        let atoms = ["a", "b", "*", "a*", "*b", "**"];
        let mut patterns: Vec<String> = Vec::new();
        for &x in &atoms {
            patterns.push(x.to_string());
            for &y in &atoms {
                patterns.push(format!("{x}/{y}"));
            }
        }
        let seg_values = ["", "a", "b", "ab", "ba"];
        let mut paths: Vec<String> = Vec::new();
        for &x in &seg_values {
            paths.push(x.to_string());
            for &y in &seg_values {
                paths.push(format!("{x}/{y}"));
                for &z in &seg_values {
                    paths.push(format!("{x}/{y}/{z}"));
                }
            }
        }
        for a in &patterns {
            for b in &patterns {
                let expect = paths.iter().all(|p| !glob_match(b, p) || glob_match(a, p));
                assert_eq!(
                    subsumes(a, b),
                    expect,
                    "subsumes({a:?}, {b:?}) disagrees with brute force"
                );
            }
        }
    }
}
