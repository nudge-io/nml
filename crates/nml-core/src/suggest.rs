//! The near-miss suggestion engine behind every "did you mean" hint.
//!
//! One metric, one threshold, one tie-break, shared by every diagnostic site
//! (enum values, oneof discriminators, unknown properties, modifiers, block
//! keywords, and the LSP's directive vocabulary), so the language's typo
//! tolerance is a single policy rather than a per-site accident.
//!
//! The policy matches rustc's `find_best_match_for_name`: a case-insensitive
//! exact match wins outright; otherwise the closest candidate by **OSA
//! (restricted Damerau-Levenshtein) distance** — a transposition costs 1,
//! because swapped adjacent letters are the dominant real-world typo — and a
//! candidate is suggested only within a third of the longer string's length
//! (minimum 1), so short values demand near-exact matches while long values
//! tolerate a typo or two. Ties break toward the earliest candidate, keeping
//! diagnostics deterministic under declaration order.
//!
//! Diagnostics-only, by invariant: a suggestion never widens what the
//! language accepts.

/// Inputs longer than this are never considered: they cannot be a near-miss
/// of any sane name under the length-proportional cutoff, so this is purely a
/// robustness guard against quadratic distance work on pathological values.
const MAX_INPUT_LEN: usize = 256;

/// The best near-miss for `input` among `candidates`, or `None` when nothing
/// is close enough to suggest.
pub fn suggest<'a, I>(input: &str, candidates: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    if input.is_empty() || input.len() > MAX_INPUT_LEN {
        return None;
    }
    let candidates: Vec<&str> = candidates.into_iter().collect();
    // The overwhelmingly common near-miss is wrong casing; it wins outright.
    if let Some(&exact) = candidates.iter().find(|c| c.eq_ignore_ascii_case(input)) {
        return Some(exact);
    }
    let input_chars: Vec<char> = input.chars().collect();
    let mut best: Option<(usize, &str)> = None;
    for &cand in &candidates {
        let cand_chars: Vec<char> = cand.chars().collect();
        let cutoff = (input_chars.len().max(cand_chars.len()) / 3).max(1);
        let dist = osa_distance(&input_chars, &cand_chars);
        // Strict `<` keeps the earliest candidate on a tie.
        if dist <= cutoff && best.is_none_or(|(b, _)| dist < b) {
            best = Some((dist, cand));
        }
    }
    best.map(|(_, cand)| cand)
}

/// Optimal string alignment (restricted Damerau-Levenshtein) distance:
/// insertions, deletions, substitutions, and adjacent transpositions, each
/// costing 1. Char-based, so multi-byte characters count as one edit.
fn osa_distance(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let cols = b.len() + 1;
    // Three-row DP: `prev2` enables the transposition case.
    let mut prev2 = vec![0usize; cols];
    let mut prev: Vec<usize> = (0..cols).collect();
    let mut curr = vec![0usize; cols];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            curr[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s<'a>(input: &str, candidates: &'a [&'a str]) -> Option<&'a str> {
        suggest(input, candidates.iter().copied())
    }

    #[test]
    fn transposition_costs_one() {
        // The bug that motivated the engine: "wran" → "warn" is a
        // transposition (plain Levenshtein 2, OSA 1) and must be suggested
        // even for a 4-char value whose cutoff is 1.
        assert_eq!(s("wran", &["debug", "info", "warn", "error"]), Some("warn"));
    }

    #[test]
    fn case_insensitive_exact_match_wins() {
        assert_eq!(s("WARN", &["warn", "wart"]), Some("warn"));
        assert_eq!(s("Restart", &["restart"]), Some("restart"));
    }

    #[test]
    fn single_edit_within_short_cutoff() {
        assert_eq!(s("dg", &["dog", "cat"]), Some("dog"));
        assert_eq!(s("lvie", &["live", "restart"]), Some("live"));
    }

    #[test]
    fn far_values_are_not_suggested() {
        assert_eq!(s("xyz", &["debug", "info"]), None);
        // Distance 2 on a 4-char value exceeds the cutoff of 1 — near-exact
        // is demanded for short names.
        assert_eq!(s("txrn", &["warn"]), None);
    }

    #[test]
    fn longer_values_tolerate_more() {
        // 10-char candidate → cutoff 3.
        assert_eq!(s("restrat", &["restart"]), Some("restart"));
        assert_eq!(s("postmrak", &["postmark", "log"]), Some("postmark"));
    }

    #[test]
    fn tie_breaks_to_earliest_candidate() {
        // Both are distance 1 from "ab"; declaration order decides,
        // deterministically.
        assert_eq!(s("ab", &["ac", "ad"]), Some("ac"));
        assert_eq!(s("ab", &["ad", "ac"]), Some("ad"));
    }

    #[test]
    fn unicode_counts_chars_not_bytes() {
        assert_eq!(s("cafe", &["café"]), Some("café"));
    }

    #[test]
    fn empty_and_pathological_inputs_are_guarded() {
        assert_eq!(s("", &["warn"]), None);
        let huge = "x".repeat(MAX_INPUT_LEN + 1);
        assert_eq!(suggest(&huge, ["warn"]), None);
        assert_eq!(s("warn", &[]), None);
    }

    #[test]
    fn osa_distance_basics() {
        let c = |x: &str| x.chars().collect::<Vec<_>>();
        assert_eq!(osa_distance(&c("warn"), &c("warn")), 0);
        assert_eq!(osa_distance(&c("wran"), &c("warn")), 1); // transposition
        assert_eq!(osa_distance(&c("warn"), &c("wart")), 1); // substitution
        assert_eq!(osa_distance(&c("arn"), &c("warn")), 1); // insertion
        assert_eq!(osa_distance(&c(""), &c("abc")), 3);
    }
}
