//! The binding-glob dialect, stated as a table.
//!
//! `nml_validate::glob` is public API and the matcher every manifest
//! `files`, `allowRefs`, `denyRefs` and `budgetUnits` glob is read
//! through. This file pins the dialect the module's grammar paragraph
//! describes, at every axis a user can reach: `**` at each position,
//! `*` inside a segment, a `**` that is not a whole segment, escaping,
//! character classes, `?`, brace expansion, a trailing separator, dot
//! segments, case and Unicode form. Each row is one sentence of the
//! specification; a row that flips is a dialect change.
use nml_validate::glob::glob_match;

/// Every row of the dialect table, asserted.
#[test]
fn the_glob_dialect_table() {
    // (pattern, path, matches)
    let table: &[(&str, &str, bool)] = &[
        // --- `**` is a whole segment matching zero or more segments ---
        ("**", "a.nml", true),
        ("**", "a/b/c.nml", true),
        ("**/*.nml", "a.nml", true),
        ("**/*.nml", "a/b.nml", true),
        ("**/*.nml", "a/b/c.nml", true),
        ("t/**", "t/a", true),
        ("t/**", "t/a/b", true),
        // `**` matches ZERO segments too, so a trailing `**` also
        // matches the prefix itself
        ("t/**", "t", true),
        ("t/**/x.nml", "t/x.nml", true),
        ("t/**/x.nml", "t/a/x.nml", true),
        ("t/**/x.nml", "t/a/b/x.nml", true),
        ("**/**/x", "x", true),
        ("**/**/x", "a/x", true),
        ("a/**/b/**/c", "a/b/c", true),
        ("a/**/b/**/c", "a/1/b/2/c", true),
        // --- `*` matches within ONE segment, never across `/` ---
        ("*.nml", "a.nml", true),
        ("*.nml", "a/b.nml", false),
        ("*", "a", true),
        ("*", "a/b", false),
        ("a*c", "ac", true),
        ("a*c", "abc", true),
        ("a*c", "abbbc", true),
        ("a*c", "ab/c", false),
        ("*a*b*", "xaybz", true),
        // --- `**` inside a segment is NOT the segment wildcard: it is
        //     two ordinary `*`, i.e. an ordinary within-segment run.
        //     The LOADER refuses these shapes (`**` must be a whole
        //     segment), so they are unreachable from a manifest — but
        //     the matcher is public API and answers this way.
        ("a**b", "ab", true),
        ("a**b", "axxb", true),
        ("a**b", "ax/xb", false),
        ("**x", "ax", true),
        ("**x", "a/x", false),
        ("x**", "xa", true),
        // --- there is NO escape character; `\` is refused as a key
        //     component, so `\*` can never match anything a key carries
        ("a\\*b", "a*b", false),
        ("a\\*b", "a\\*b", true),
        // a literal `*` in a name is UNMATCHABLE as a literal
        ("*", "*", true),
        ("a*", "a*", true),
        // --- no character classes, no `?`, no brace expansion:
        //     each is an ordinary literal
        ("[ab].nml", "a.nml", false),
        ("[ab].nml", "[ab].nml", true),
        ("?.nml", "a.nml", false),
        ("?.nml", "?.nml", true),
        ("{a,b}.nml", "a.nml", false),
        ("{a,b}.nml", "{a,b}.nml", true),
        // --- a trailing separator makes an empty final segment, which
        //     no key component can equal: the pattern matches NOTHING
        ("t/", "t", false),
        ("t/", "t/a", false),
        // --- a leading separator and dot segments likewise: keys are
        //     root-relative and carry no `.`/`..`
        ("/t/**", "t/a", false),
        ("./t/**", "t/a", false),
        ("t/./**", "t/a", false),
        ("a/../b", "b", false),
        // --- matching is CASE-SENSITIVE and form-sensitive (the key
        //     carries the on-disk spelling; E25)
        ("admin/**", "Admin/x", false),
        ("Admin/**", "Admin/x", true),
        ("caf\u{e9}/**", "caf\u{e9}/x", true),
        ("caf\u{e9}/**", "cafe\u{301}/x", false),
        // --- a dot-name is an ordinary component to the MATCHER (the
        //     WALK is what never enumerates one)
        ("**/*", ".hidden.nml", true),
        ("*", ".hidden", true),
        ("t/**", "t/.h/x", true),
    ];
    let mut wrong = Vec::new();
    for &(pattern, path, want) in table {
        let got = glob_match(pattern, path);
        if got != want {
            wrong.push(format!("{pattern:?} vs {path:?}: want {want}, got {got}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "dialect rows changed:\n  {}",
        wrong.join("\n  ")
    );
}

/// The two caps the matcher fences itself with answer `false` for every
/// path rather than spending the work (the loader refuses both shapes,
/// so a live grant never carries one).
#[test]
fn a_pattern_past_either_cap_matches_nothing() {
    let deep = vec!["*"; nml_validate::glob::MAX_PATTERN_SEGMENTS + 1].join("/");
    let path = vec!["a"; nml_validate::glob::MAX_PATTERN_SEGMENTS + 1].join("/");
    assert!(!glob_match(&deep, &path), "past the segment cap");
    let long = "a".repeat(nml_validate::glob::MAX_PATTERN_SEGMENT_BYTES + 1);
    assert!(!glob_match(&long, &long), "past the segment-byte cap");
}
