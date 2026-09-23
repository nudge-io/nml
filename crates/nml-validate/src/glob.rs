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

/// A plain entry name: what a listing yields, what a key component may be,
/// and what one pattern segment may equal — never empty, `.`, `..`, or
/// separator-bearing. ONE segment vocabulary, and it lives with the matcher:
/// the loader (`package::glob_rule`, refusing a glob segment no key component
/// can equal) and the walk (`workspace::paths`, minting the components) both
/// read it DOWNWARD — the loader used to reach up into the walk for it.
pub(crate) fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Patterns beyond this segment count are rejected outright (matching
/// returns `false`). Binding patterns are publisher-authored layout
/// conventions a handful of segments deep; the cap exists so a hostile
/// manifest cannot feed the matcher pathological input (the LSP runs this
/// per keystroke on attacker-controlled workspaces).
///
/// LIMIT: reach=content guards=work surface=kernel shown="64" — segments in one manifest `files` glob
pub const MAX_PATTERN_SEGMENTS: usize = 64;

/// Bound on ONE pattern segment's bytes — the companion the segment
/// COUNT cap was missing. A segment matches ONE path component, and no
/// filesystem admits a component past 255 bytes, so the longest segment
/// that can match anything a key carries is 255 literal characters with
/// a `*` between each: 511 bytes. This leaves twice that.
///
/// Without it the count cap bounded the number of segments and the
/// manifest cap the SUM of their lengths, but nothing bounded ONE of
/// them — and the matcher's cost is the segment's length times the path
/// components it is tried against, per file. Measured before this bound
/// (200 two-line files under `**/<250 KB segment>` at depth 59, a
/// manifest of 244 KiB, inside every published bound): 48.9 s wall,
/// 32.7 s CPU, against 0.2 s for the same tree under `tenants/**`.
///
/// LIMIT: reach=content guards=work surface=kernel shown="1 KiB" — bytes of ONE segment of a manifest glob (`files`, `allowRefs`, `denyRefs`, `budgetUnits`)
pub const MAX_PATTERN_SEGMENT_BYTES: usize = 1024;

/// Whether the matcher will spend work on this pattern: at most
/// [`MAX_PATTERN_SEGMENTS`] segments, none past
/// [`MAX_PATTERN_SEGMENT_BYTES`]. Past either, matching answers `false`
/// for every path — the matcher's OWN fence, which is why the loader
/// refuses both shapes at load (`package::glob_rule`,
/// `package::validate_budget_units`): a pattern that matches nothing is
/// fail-closed for `files` and an allow rule and FAIL-OPEN for a deny
/// rule, so no such pattern may ever reach a live grant.
fn matchable(pat: &[&str]) -> bool {
    pat.len() <= MAX_PATTERN_SEGMENTS && pat.iter().all(|s| s.len() <= MAX_PATTERN_SEGMENT_BYTES)
}

/// Bound on the subsumption product walk's explored state-pair count. The
/// subset construction over a `**`+`*`-heavy pattern is exponential in the
/// worst case — correctness proofs say nothing about complexity, and this
/// runs per keystroke on attacker-supplied manifests. On exceed, `subsumes`
/// returns `false` ("incomparable"), which only suppresses an authoring
/// warning — always safe.
///
/// LIMIT: reach=content guards=work surface=kernel shown="10000" — states explored when deciding whether one glob subsumes another
const MAX_SUBSUMES_STATES: usize = 10_000;

/// The unit boundary of a claiming glob (RFC 0019 A16 amendment): the
/// index of the segment that STARTS THE LAST RUN of
/// consecutive wildcard DIRECTORY segments — the directories that
/// segment matches are the budget units the amendment bounds
/// separately. The final segment names files, not directories, and
/// never induces a unit unless it is `**` (which reaches directories at
/// every depth); adjacent wildcards (`*/**`, `**/*`) are ONE boundary at
/// their first segment, so a tenant's own subdirectories are never
/// units of their own. `None` when no directory segment bears a
/// wildcard: a wildcard-free glob is the operator's layout end to end,
/// and `*.flow.nml` reaches no directory at all.
///
/// Why the LAST run and not the first: a literal
/// segment AFTER a wildcard is still the operator's layout.
/// `orgs/*/tenants/**` fixes a `tenants` directory inside every org, so
/// delegated content begins under `orgs/<o>/tenants/<t>`, not at
/// `orgs/<o>` — where the first-wildcard rule put the unit, so that one
/// tenant's spam denied every sibling tenant in its org. Why not "the
/// first wildcard segment with no literal after it":
/// `tenants/*/flows/*.flow.nml` has a literal after its only
/// directory wildcard and would infer NO unit at all, regressing that
/// layout to the whole-universe denial the amendment exists to prevent.
/// The last run keeps every pinned row and settles both
/// (`unit_inference_rules_compared` executes all three rules over the
/// same layouts).
pub fn unit_prefix_len(pattern: &str) -> Option<usize> {
    let segments: Vec<&str> = pattern.split('/').collect();
    let directories = if segments.last() == Some(&"**") {
        segments.len()
    } else {
        segments.len().saturating_sub(1)
    };
    let mut start = None;
    let mut in_run = false;
    for (i, segment) in segments[..directories].iter().enumerate() {
        if segment.contains('*') {
            if !in_run {
                start = Some(i);
                in_run = true;
            }
        } else {
            in_run = false;
        }
    }
    start
}

/// The delegation GAP of a binding glob under unit inference (RFC 0019
/// E38, item 4): a glob whose first wildcard directory run is not its
/// last — `tenants/*/flows/**` — starts delegating at `tenants/<x>`
/// while [`unit_prefix_len`] infers the unit at `tenants/<x>/flows/<y>`,
/// so `tenants/<x>/other/` is the ROOT unit's. `None` for a glob with no
/// wildcard directory, or whose only run is its last (no gap).
/// `delegated` is the shallowest unit that still covers the glob
/// (`tenants/*`), `inferred` the inferred one (`tenants/*/flows/*`,
/// a `**` at the boundary spelled `*`), and `unbounded` says a `**`
/// sits BEFORE the boundary (`tenants/**/flows/**`, `**/tenants/*/**`):
/// no fixed depth can be declared until the layout is respelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitGap {
    pub delegated: String,
    pub inferred: String,
    pub unbounded: bool,
}

pub fn unit_gap(pattern: &str) -> Option<UnitGap> {
    let boundary = unit_prefix_len(pattern)?;
    let segments: Vec<&str> = pattern.split('/').collect();
    let directories = if segments.last() == Some(&"**") {
        segments.len()
    } else {
        segments.len().saturating_sub(1)
    };
    let dirs = &segments[..directories];
    let first = dirs.iter().position(|s| s.contains('*'))?;
    if first == boundary {
        return None;
    }
    Some(UnitGap {
        delegated: unit_pattern(&dirs[..=first]),
        inferred: unit_pattern(&dirs[..=boundary]),
        unbounded: dirs[..boundary].contains(&"**"),
    })
}

/// The budget unit a claiming glob INFERS (RFC 0019 E38), as a unit
/// pattern: its directory segments up to and including the
/// [`unit_prefix_len`] boundary, every wildcard-bearing segment spelled
/// `*` (a `**` at the boundary is the delegation point itself). `None`
/// for a glob that infers no unit, or whose wildcards have no fixed
/// depth above the boundary (a `**` before it): no pattern names it.
pub fn inferred_unit(pattern: &str) -> Option<String> {
    let boundary = unit_prefix_len(pattern)?;
    let segments: Vec<&str> = pattern.split('/').collect();
    let dirs = &segments[..=boundary];
    if dirs[..boundary].contains(&"**") {
        return None;
    }
    Some(unit_pattern(dirs))
}

/// Directory segments spelled as a UNIT pattern — the vocabulary
/// `budgetUnits` declares in: a wildcard-bearing segment is `*` (a unit
/// names a directory level, never a name shape), a literal is itself.
/// The ONE spelling [`UnitGap`]'s two units and [`inferred_unit`] share.
fn unit_pattern(dirs: &[&str]) -> String {
    dirs.iter()
        .map(|s| if s.contains('*') { "*" } else { *s })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether the unit pattern `inner` nests inside `outer` in the one way
/// that multiplies a delegate's share of the walk's budget (RFC 0026
/// B-3): `outer` is a segment-wise prefix of `inner` (a wildcard segment
/// overlaps anything; a literal only itself) and NO segment of `inner`
/// PINS one of `outer`'s wildcards to a literal. Pinning is the operator
/// naming a directory of their own — `*` beside `tenants/*` (a root
/// catch-all's unit beside the tenants', E38's designed nesting) is
/// `tenants` pinned, and stands; `tenants/*` beside `tenants/*/*` (or
/// `tenants/t-*/x`) is every delegate minting units beneath itself, and
/// is refused. A unit is never inside a wider one at equal depth, and a
/// literal unit (`tenants/cu`) is inside nothing that does not name it.
pub fn unit_nests_inside(outer: &str, inner: &str) -> bool {
    let outer: Vec<&str> = outer.split('/').collect();
    let inner: Vec<&str> = inner.split('/').collect();
    if outer.len() >= inner.len() {
        return false;
    }
    let wild = |s: &str| s.contains('*');
    let inside = outer
        .iter()
        .zip(&inner)
        .all(|(o, i)| wild(o) || wild(i) || o == i);
    let pinned = outer.iter().zip(&inner).any(|(o, i)| wild(o) && !wild(i));
    inside && !pinned
}

/// Whether `pattern` can match ANY path under the directory `dir` (a
/// `/`-normalized relative directory path; `""` is the root) — the
/// "claimed content" test of RFC 0019's *Resolution inputs must not be
/// author-writable*: a project config, manifest or marker that lives
/// inside content a binding's globs reach is content, not configuration.
/// `tenants/**/*.flow.nml` reaches `tenants/cu` (it claims files there)
/// without matching `tenants/cu/nml-project.nml` itself — the exact
/// case a match-the-file test misses. Same DP as [`glob_match`], asking
/// whether some PROPER prefix of the pattern consumes all of `dir`'s
/// segments (the remainder then matches at least one more segment).
pub fn glob_reaches_dir(pattern: &str, dir: &str) -> bool {
    let mut pat: Vec<&str> = Vec::new();
    for seg in pattern.split('/') {
        if seg == "**" && pat.last() == Some(&"**") {
            continue;
        }
        pat.push(seg);
    }
    if !matchable(&pat) {
        return false;
    }
    let segs: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    let seg_chars = chars_of(&segs);
    // dp[j] = does pat[..i] match segs[..j]; a proper prefix (i < len)
    // matching every segment means the pattern continues below `dir`.
    let mut dp = vec![false; segs.len() + 1];
    dp[0] = true;
    for (i, &p) in pat.iter().enumerate() {
        if dp[segs.len()] && i < pat.len() {
            return true;
        }
        let mut next = vec![false; segs.len() + 1];
        if p == "**" {
            let mut any = false;
            for j in 0..=segs.len() {
                any |= dp[j];
                next[j] = any;
            }
        } else {
            // The segment's characters ONCE per pattern row, not once
            // per cell: the row visits every path component, and a
            // `Vec<char>` inside the cell made one comparison cost the
            // whole segment even where its first character decided it.
            let p_chars: Vec<char> = p.chars().collect();
            for j in 1..=segs.len() {
                next[j] = dp[j - 1] && match_segment(&p_chars, &seg_chars[j - 1]);
            }
        }
        dp = next;
    }
    // The whole pattern consumed `dir`: only a trailing `**` (zero or
    // MORE segments) can still descend below it.
    dp[segs.len()] && pat.last() == Some(&"**")
}

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
    if !matchable(&pat) {
        return false;
    }
    let segs: Vec<&str> = path.split('/').collect();
    let seg_chars = chars_of(&segs);

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
            let p_chars: Vec<char> = p.chars().collect();
            for j in 1..=segs.len() {
                next[j] = dp[j - 1] && match_segment(&p_chars, &seg_chars[j - 1]);
            }
        }
        dp = next;
    }
    dp[segs.len()]
}

/// Each path component's characters, once per match — the DP tries every
/// component against every pattern row.
fn chars_of(segs: &[&str]) -> Vec<Vec<char>> {
    segs.iter().map(|s| s.chars().collect()).collect()
}

/// `*`-wildcard match within a single segment (no `/` crossing by
/// construction: segments are already split). Takes both sides already
/// as characters: the caller materializes each exactly once per match,
/// never once per DP cell.
fn match_segment(pat: &[char], seg: &[char]) -> bool {
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
    use super::{
        MAX_PATTERN_SEGMENT_BYTES, MAX_PATTERN_SEGMENTS, glob_match, glob_reaches_dir,
        inferred_unit, unit_gap, unit_nests_inside, unit_prefix_len,
    };

    /// r103-sec F3: ONE segment's length is bounded like the segment
    /// COUNT is, and for the same reason — the matcher tries a segment
    /// against every path component of every file, so an unbounded one
    /// buys unbounded work per file from inside the 256 KiB manifest
    /// cap. Past the bound the pattern matches NOTHING (the matcher's
    /// own fence), which is why `package::glob_rule` refuses it at load:
    /// a pattern that matches nothing is fail-open for a `denyRefs`
    /// veto. Measured before the bound: 200 two-line files under
    /// `**/<250 KB segment>` at depth 59 cost 48.9 s wall / 32.7 s CPU;
    /// after, the manifest is refused at load and the run is 0.04 s.
    #[test]
    fn one_segment_past_the_byte_bound_matches_nothing() {
        let at = "*".repeat(MAX_PATTERN_SEGMENT_BYTES);
        let past = "*".repeat(MAX_PATTERN_SEGMENT_BYTES + 1);
        assert_eq!(at.len(), MAX_PATTERN_SEGMENT_BYTES);
        // A run of `*` matches any single component — at the bound.
        assert!(glob_match(&format!("tenants/{at}"), "tenants/cu"));
        assert!(glob_reaches_dir(&format!("tenants/{at}/**"), "tenants/cu"));
        // One byte past it, nothing matches, wherever the segment sits.
        assert!(!glob_match(&format!("tenants/{past}"), "tenants/cu"));
        assert!(!glob_match(&format!("{past}/x"), "tenants/x"));
        assert!(!glob_reaches_dir(
            &format!("tenants/{past}/**"),
            "tenants/cu"
        ));
        // A multi-byte segment is bounded in BYTES, not characters, and
        // the fence never slices one (a `char` cut would panic).
        let wide = "\u{1f600}".repeat(MAX_PATTERN_SEGMENT_BYTES / 4 + 1);
        assert!(wide.len() > MAX_PATTERN_SEGMENT_BYTES);
        assert!(!glob_match(&format!("tenants/{wide}"), "tenants/cu"));
        // And the count bound still stands beside it.
        let deep = vec!["a"; MAX_PATTERN_SEGMENTS + 1].join("/");
        assert!(!glob_match(&deep, &deep));
    }

    /// RFC 0026 B-3: the inferred unit as a pattern, and the one nesting
    /// that multiplies — pinned nestings (E38's root catch-all beside
    /// the tenants) stand.
    #[test]
    fn inferred_units_and_the_nesting_rule() {
        assert_eq!(
            inferred_unit("tenants/**/*.flow.nml").as_deref(),
            Some("tenants/*")
        );
        assert_eq!(inferred_unit("**/*.model.nml").as_deref(), Some("*"));
        assert_eq!(
            inferred_unit("tenants/*/plugins/*/**/*.model.nml").as_deref(),
            Some("tenants/*/plugins/*")
        );
        assert_eq!(
            inferred_unit("orgs/*/tenants/**").as_deref(),
            Some("orgs/*/tenants/*")
        );
        assert_eq!(inferred_unit("admin/ops.flow.nml"), None, "wildcard-free");
        assert_eq!(inferred_unit("tenants/**/flows/**"), None, "no fixed depth");
        for (outer, inner) in [
            ("tenants/*", "tenants/*/*"),
            ("tenants/*", "tenants/*/plugins/*"),
            ("tenants/*", "tenants/t-*/x"),
            ("tenants/cu", "tenants/*/*"),
            ("orgs/*", "orgs/*/tenants/*"),
            ("*", "*/x/*"),
        ] {
            assert!(unit_nests_inside(outer, inner), "{outer} < {inner}");
            assert!(!unit_nests_inside(inner, outer), "{inner} < {outer}");
        }
        for (outer, inner) in [
            ("*", "tenants/*"),
            ("tenants/*", "tenants/ops/*"),
            ("tenants/*", "vendor/*"),
            ("tenants/*", "tenants/*"),
            ("tenants/cu", "tenants/du/*"),
        ] {
            assert!(!unit_nests_inside(outer, inner), "{outer} < {inner}");
        }
    }

    /// r89 (P11): the gap shapes E38 names, and the silent ones.
    #[test]
    fn unit_gap_names_the_delegated_and_inferred_units() {
        let gap = |p: &str| unit_gap(p).map(|g| (g.delegated, g.inferred, g.unbounded));
        assert_eq!(
            gap("tenants/*/flows/**"),
            Some(("tenants/*".into(), "tenants/*/flows/*".into(), false))
        );
        assert_eq!(
            gap("tenants/*/flows/**/*.flow.nml"),
            Some(("tenants/*".into(), "tenants/*/flows/*".into(), false))
        );
        assert_eq!(
            gap("orgs/*/tenants/**"),
            Some(("orgs/*".into(), "orgs/*/tenants/*".into(), false))
        );
        assert_eq!(
            gap("tenants/**/flows/**"),
            Some(("tenants/*".into(), "tenants/*/flows/*".into(), true))
        );
        assert_eq!(
            gap("**/tenants/*/**"),
            Some(("*".into(), "*/tenants/*".into(), true))
        );
        for quiet in [
            "tenants/**/*.flow.nml",
            "tenants/*/flows/*.flow.nml",
            "tenants/**",
            "**",
            "**/*.nml",
            "admin/ops.flow.nml",
            "*.flow.nml",
        ] {
            assert_eq!(unit_gap(quiet), None, "{quiet}");
        }
    }
    /// The three candidate inference rules over the layouts an operator
    /// plausibly writes, side by side, each row EXECUTED: `first` is the
    /// first wildcard segment, `no_literal_after` the one-liner (the
    /// first wildcard segment with no literal segment after it), and
    /// `last_run` is [`unit_prefix_len`]. A cell is whether `dir` is a
    /// unit root under that rule — the boundary index plus the same
    /// `glob_reaches_dir` test `is_budget_unit` applies.
    #[test]
    fn unit_inference_rules_compared() {
        fn first(p: &str) -> Option<usize> {
            let n = p.split('/').take_while(|s| !s.contains('*')).count();
            (n < p.split('/').count()).then_some(n)
        }
        fn no_literal_after(p: &str) -> Option<usize> {
            let s: Vec<&str> = p.split('/').collect();
            (0..s.len()).find(|&i| s[i].contains('*') && s[i + 1..].iter().all(|t| t.contains('*')))
        }
        fn unit_at(rule: fn(&str) -> Option<usize>, glob: &str, dir: &str) -> bool {
            let depth = dir.split('/').count();
            rule(glob) == Some(depth - 1) && glob_reaches_dir(glob, dir)
        }
        // (glob, directory, first, no_literal_after, last_run)
        let rows: &[(&str, &str, bool, bool, bool)] = &[
            // The RFC's own spellings: every rule agrees.
            ("tenants/**/*.flow.nml", "tenants/cu", true, true, true),
            (
                "tenants/**/*.flow.nml",
                "tenants/cu/nested",
                false,
                false,
                false,
            ),
            ("tenants/**", "tenants/cu", true, true, true),
            ("tenants/*/**", "tenants/cu", true, true, true),
            ("tenants/*/**", "tenants/cu/sub", false, false, false),
            ("**/*.flow.nml", "admin", true, true, true),
            ("**", "admin", true, true, true),
            ("*", "admin", false, false, false),
            ("*.flow.nml", "admin", false, false, false),
            ("admin/ops.flow.nml", "admin", false, false, false),
            ("tenants/prod/**", "tenants/prod/cu", true, true, true),
            // The surprise: a literal INSIDE the delegated namespace.
            ("orgs/*/tenants/**", "orgs/acme", true, false, false),
            (
                "orgs/*/tenants/**",
                "orgs/acme/tenants/cu",
                false,
                true,
                true,
            ),
            // Operator layout INSIDE each tenant: the one-liner
            // infers no unit at all and regresses the layout to the
            // whole-universe denial; `first` and `last_run` agree.
            (
                "tenants/*/flows/*.flow.nml",
                "tenants/cu",
                true,
                false,
                true,
            ),
            (
                "tenants/*/flows/*.flow.nml",
                "tenants/cu/flows",
                false,
                false,
                false,
            ),
            (
                "tenants/**/flows/*.flow.nml",
                "tenants/cu",
                true,
                false,
                true,
            ),
            // Both shapes at once: only the last run reaches the tenant.
            (
                "orgs/*/tenants/*/flows/*.flow.nml",
                "orgs/acme",
                true,
                false,
                false,
            ),
            (
                "orgs/*/tenants/*/flows/*.flow.nml",
                "orgs/acme/tenants/cu",
                false,
                false,
                true,
            ),
            // Three delegated levels: the deepest is the unit.
            ("a/*/b/*/c/**", "a/x", true, false, false),
            ("a/*/b/*/c/**", "a/x/b/y", false, false, false),
            ("a/*/b/*/c/**", "a/x/b/y/c/z", false, true, true),
        ];
        for (glob, dir, want_first, want_nla, want_last) in rows {
            assert_eq!(
                unit_at(first, glob, dir),
                *want_first,
                "first: {glob} over {dir}"
            );
            assert_eq!(
                unit_at(no_literal_after, glob, dir),
                *want_nla,
                "no_literal_after: {glob} over {dir}"
            );
            assert_eq!(
                unit_at(unit_prefix_len, glob, dir),
                *want_last,
                "last_run: {glob} over {dir}"
            );
        }
    }

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
        // The segment cap, exactly: one over is rejected outright;
        // at the cap a pattern still matches its own path.
        let long_pattern = vec!["a"; MAX_PATTERN_SEGMENTS + 1].join("/");
        assert!(
            !glob_match(&long_pattern, &long_pattern),
            "over-cap patterns are rejected"
        );
        let at_cap = vec!["a"; MAX_PATTERN_SEGMENTS].join("/");
        assert!(glob_match(&at_cap, &at_cap), "at the cap a pattern matches");
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
    subsumes_within(a, b, MAX_SUBSUMES_STATES).unwrap_or(false)
}

/// [`subsumes`] drawing on a budget SHARED with every other pair in the
/// same analysis, spending from `remaining` the work the walk did.
///
/// [`MAX_SUBSUMES_STATES`] bounds ONE comparison; it bounds no analysis
/// that makes many. A caller comparing every glob against every earlier
/// one does quadratically many comparisons, so a declaration list whose
/// own byte bound admits thousands of globs costs that bound TIMES the
/// square — measured at 108 s for 447 declarations in a 64 KiB input,
/// and quadratic from there. A shared budget is what makes the ANALYSIS
/// bounded rather than each of its steps: past it every remaining pair
/// answers `false` (incomparable), which only suppresses an authoring
/// warning, exactly as a single exhausted comparison does.
///
/// The currency is [`subsumes_cost`], not state pairs: one state pair
/// over two 64 KiB patterns is a thousand times the work of one over
/// two 30-byte patterns, so a budget counted in state pairs bounds
/// nothing a pattern's own length can inflate.
pub(crate) fn subsumes_budgeted(a: &str, b: &str, remaining: &mut usize) -> bool {
    if *remaining == 0 {
        return false;
    }
    let (verdict, spent) = subsumes_counted(a, b, MAX_SUBSUMES_STATES, *remaining);
    *remaining = remaining.saturating_sub(spent);
    verdict.unwrap_or(false)
}

/// [`subsumes`] under an explicit state budget: `None` when the product
/// walk GAVE UP at `max_states` (incomparable — the caller's `false`),
/// `Some` when it decided. The budget is a parameter so a test can pin
/// what [`MAX_SUBSUMES_STATES`] buys — that a pathological pair is
/// answered by the budget, not by the automata — instead of only timing
/// it.
fn subsumes_within(a: &str, b: &str, max_states: usize) -> Option<bool> {
    subsumes_counted(a, b, max_states, usize::MAX).0
}

/// What one comparison COST, in the currency a shared budget is spent
/// from ([`subsumes_budgeted`]): the state pairs the walk explored times
/// the two automata's size. Every step of the walk is a pass over both
/// state vectors, so that product — not the state count — is the work
/// done, and it is what a long pattern inflates.
fn subsumes_cost(states: usize, automata: usize) -> usize {
    states.saturating_mul(automata).max(1)
}

/// [`subsumes_within`] plus the work it did ([`subsumes_cost`]) — what a
/// shared budget is spent from ([`subsumes_budgeted`]). A pair refused
/// on its segment count alone costs the minimum, never zero: a budget
/// nothing can spend is no budget.
fn subsumes_counted(a: &str, b: &str, max_states: usize, max_work: usize) -> (Option<bool>, usize) {
    if a.split('/').count() > MAX_PATTERN_SEGMENTS || b.split('/').count() > MAX_PATTERN_SEGMENTS {
        return (Some(false), 1);
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

    // The state cap the WORK budget affords, once the automata are
    // built and their size is known: a pair may not spend more than the
    // analysis has left, so one maximal pattern cannot buy itself a full
    // per-comparison budget at a thousand times the price per state.
    // At least one state, so a pair that decides on its first is never
    // priced out of deciding.
    let automata = na.edges.len() + nb.edges.len();
    let max_states = max_states.min((max_work / automata.max(1)).max(1));
    let mut seen = std::collections::HashSet::new();
    let mut queue = vec![(sb, sa)];
    while let Some((cb, ca)) = queue.pop() {
        if cb[nb.accept] && !ca[na.accept] {
            return (Some(false), subsumes_cost(seen.len(), automata));
        }
        if seen.len() >= max_states {
            // Complexity bound, not a correctness statement: give up on
            // comparing pathological patterns rather than hang the server.
            return (None, subsumes_cost(seen.len(), automata));
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
    (Some(true), subsumes_cost(seen.len(), automata))
}

#[cfg(test)]
mod subsumption_tests {
    use super::{
        MAX_PATTERN_SEGMENTS, MAX_SUBSUMES_STATES, glob_match, subsumes, subsumes_budgeted,
        subsumes_counted, subsumes_within,
    };

    /// The SHARED budget: it is spent down by every pair, it answers
    /// `false` for every pair once it is gone, and its currency
    /// accounts for the patterns' SIZE — a budget counted in state
    /// pairs alone would let one 64 KiB pattern do a thousand times the
    /// work of a 30-byte one for the same price, which is the whole
    /// reason `subsumes_cost` multiplies.
    #[test]
    fn the_shared_budget_is_spent_down_and_charges_for_pattern_size() {
        let mut budget = usize::MAX;
        assert!(subsumes_budgeted("**", "a/**/b", &mut budget));
        let spent_small = usize::MAX - budget;
        assert!(spent_small > 0, "a decided pair costs something");

        // The same state count over LONGER patterns costs more.
        let long = format!("{}/**/{}", "a".repeat(512), "b".repeat(512));
        let (_, big) = subsumes_counted("**", &long, MAX_SUBSUMES_STATES, usize::MAX);
        let (_, small) = subsumes_counted("**", "a/**/b", MAX_SUBSUMES_STATES, usize::MAX);
        assert!(
            big > small * 10,
            "a long pattern must cost more per pair: {big} vs {small}"
        );

        // An exhausted budget answers `false` for every later pair —
        // withholding an advisory, never inventing one — and spends
        // nothing more.
        let mut spent = 0usize;
        assert!(!subsumes_budgeted("**", "a/**/b", &mut spent));
        assert_eq!(spent, 0, "an exhausted budget does no work at all");
        // And it does not even BUILD the automata. That is the whole
        // value of the early return: after the budget is gone the
        // analysis still walks its O(N²) pairs, and constructing two
        // 64 KiB NFAs per pair is work whatever the walk then decides.
        // The loop below is supposed to cost NOTHING, so a small
        // absolute is a statement about zero, not a perf pin — three
        // orders of magnitude of headroom either way.
        let maximal = vec!["*x".repeat(512); MAX_PATTERN_SEGMENTS].join("/");
        let mut gone = 0usize;
        let start = std::time::Instant::now();
        for _ in 0..200 {
            assert!(!subsumes_budgeted(&maximal, &maximal, &mut gone));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "an exhausted budget must return before building anything: {:?} for 200 calls",
            start.elapsed()
        );

        // A pair refused on its segment count still costs: a budget
        // nothing can spend is no budget.
        let deep = vec!["a"; MAX_PATTERN_SEGMENTS + 1].join("/");
        let mut budget = 2usize;
        assert!(!subsumes_budgeted(&deep, &deep, &mut budget));
        assert_eq!(budget, 1);
    }

    /// The state budget is what bounds the walk —
    /// a comparable pair decides well inside [`MAX_SUBSUMES_STATES`] and
    /// gives up under a budget of one, and [`subsumes`] renders a
    /// give-up as `false` (a suppressed warning, never a wrong answer).
    #[test]
    fn the_state_budget_is_what_bounds_the_walk() {
        assert_eq!(
            subsumes_within("**", "a/**/b", MAX_SUBSUMES_STATES),
            Some(true)
        );
        assert_eq!(
            subsumes_within("a/b", "a/*", MAX_SUBSUMES_STATES),
            Some(false)
        );
        assert_eq!(
            subsumes_within("**", "a/**/b", 1),
            None,
            "a budget of one gives up"
        );
        // A give-up renders as `false` — an incomparable pair only ever
        // suppresses a warning — whatever the budget would have decided.
        assert!(subsumes("**", "a/**/b"));
        let star_run = format!("**/{}", ["*"; 12].join("/"));
        assert_eq!(subsumes_within(&star_run, "a/b", 1), None);
        assert!(!subsumes_within(&star_run, "a/b", 1).unwrap_or(false));
    }

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

#[cfg(test)]
mod reach_tests {
    use super::glob_reaches_dir;

    #[test]
    fn reach_is_prefix_matching_with_a_continuation() {
        assert!(glob_reaches_dir("tenants/**/*.flow.nml", "tenants/cu"));
        assert!(glob_reaches_dir("tenants/**/*.flow.nml", "tenants"));
        assert!(glob_reaches_dir("tenants/**/*.flow.nml", "tenants/cu/deep"));
        assert!(glob_reaches_dir("tenants/**/*.flow.nml", ""), "the root");
        assert!(!glob_reaches_dir("tenants/**/*.flow.nml", "vendor"));
        assert!(!glob_reaches_dir("tenants/**/*.flow.nml", "tenantsx"));
        assert!(glob_reaches_dir("apps/*/app.nml", "apps/x"));
        assert!(
            !glob_reaches_dir("apps/*/app.nml", "apps/x/y"),
            "nothing below the file"
        );
        assert!(
            !glob_reaches_dir("demo.nml", "demo.nml"),
            "a file is not a directory"
        );
        assert!(glob_reaches_dir("**", "any/where"));
        assert!(glob_reaches_dir("**/*.nml", "a/b/c"));
        assert!(
            !glob_reaches_dir("a/b", "a/b"),
            "the whole pattern consumed: no continuation"
        );
        assert!(
            glob_reaches_dir("tenants/**", "tenants/cu"),
            "a trailing `**` descends"
        );
        assert!(glob_reaches_dir("tenants/**", "tenants"));
        assert!(!glob_reaches_dir("tenants/**", "vendor"));
        let huge = (0..65).map(|_| "a").collect::<Vec<_>>().join("/");
        assert!(
            !glob_reaches_dir(&huge, "a"),
            "over-cap patterns reach nothing"
        );
    }
}
