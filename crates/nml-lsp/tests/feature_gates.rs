//! The `test-support` feature is ADDITIVE, and this is what keeps it so.
//!
//! Two crates carry a `test-support` feature and both are enabled by
//! DEV-dependencies — `nml-validate`'s by the CLI and the fuzz targets,
//! this crate's by its own integration tests (a self dev-dependency) and
//! by the CLI's parity test. Cargo resolves features per PACKAGE, so a
//! build that includes any dev target compiles that package's ordinary
//! targets with the feature on as well: under `cargo test` the `nml-lsp`
//! BINARY that the stdio end-to-end test drives is built with
//! `test-support`, while the binary a release build ships is not. That is
//! safe for exactly as long as the feature can add NAMES and nothing else.
//! A `cfg(not(feature = "test-support"))` arm, or a `cfg!(feature =
//! "test-support")` branch inside a shared body, would make the binary
//! under test behave differently from the binary that ships — and no test
//! could see the difference, because every test runs the first one.
//!
//! So the feature may appear in exactly two attribute spellings, on
//! exactly a `mod`, a `use`, an `fn` or an `impl` — never negated, never as
//! a `cfg!` expression, never on a field or a variant (which would reshape
//! a public type under the feature). Every occurrence in the shipped
//! sources of every crate is checked; a new one is either one of the two
//! spellings or a decision somebody has to make out loud.

use std::path::PathBuf;

use nml_validate::test_support::scan;

/// The feature this file is about.
const FEATURE: &str = "test-support";

/// The only two spellings, with whitespace squeezed out and the string
/// literal blanked (the shared lexer blanks every literal, quotes included).
const ALLOWED: [&str; 2] = ["#[cfg(feature=)]", "#[cfg(any(test,feature=))]"];

/// What the attribute may gate: a whole ITEM — never a field, a variant, a
/// statement or an expression, each of which would change a shape or a
/// behaviour the feature is not allowed to touch. An item head is a
/// keyword; a field and a variant begin with a name.
const ITEM_HEADS: [&str; 13] = [
    "mod",
    "use",
    "fn",
    "impl",
    "struct",
    "enum",
    "trait",
    "type",
    "const",
    "static",
    "union",
    "extern",
    "macro_rules",
];

/// Every shipped `.rs` source of the workspace: each crate's `src/` and the
/// CLI's. `sources` skips `tests` directories and `target`.
fn shipped_sources() -> Vec<PathBuf> {
    let root = scan::workspace();
    let mut out = Vec::new();
    scan::sources(&root.join("crates"), false, &mut out);
    scan::sources(&root.join("nml-cli"), false, &mut out);
    out.sort();
    out
}

/// The attribute (as char indices of the blanked text) enclosing `at`, or
/// `None` when nothing does.
fn enclosing_attribute(clean: &[char], at: usize) -> Option<(usize, usize)> {
    let open = (0..at)
        .rev()
        .find(|i| clean[*i] == '#' && clean.get(i + 1) == Some(&'['))?;
    let mut depth = 0usize;
    for (i, c) in clean.iter().enumerate().skip(open + 1) {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return (i > at).then_some((open, i));
                }
            }
            _ => {}
        }
    }
    None
}

/// The word an attribute gates: further attributes are stepped over, `pub`
/// and `pub(crate)` are stepped over, and the first word after them is it.
fn gated_word(clean: &[char], from: usize) -> String {
    let mut i = from;
    loop {
        while clean.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        if clean.get(i) == Some(&'#') && clean.get(i + 1) == Some(&'[') {
            let mut depth = 0usize;
            while i < clean.len() {
                match clean[i] {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        let mut word = String::new();
        while clean
            .get(i)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
        {
            word.push(clean[i]);
            i += 1;
        }
        if word == "pub" {
            if clean.get(i) == Some(&'(') {
                while i < clean.len() && clean[i] != ')' {
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        return word;
    }
}

/// The 1-based line a char index sits on.
fn line_of(text: &str, at: usize) -> usize {
    text.chars().take(at).filter(|c| *c == '\n').count() + 1
}

#[test]
fn the_test_support_feature_only_adds_names() {
    let mut findings: Vec<String> = Vec::new();
    let mut seen = 0usize;
    let mut crates_seen: Vec<String> = Vec::new();
    for path in shipped_sources() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !text.contains(FEATURE) {
            continue;
        }
        let mut literals: Vec<(usize, String)> = Vec::new();
        let clean: Vec<char> = scan::lex(&text, &mut |at, s| literals.push((at, s)))
            .chars()
            .collect();
        for (at, literal) in literals {
            if literal != FEATURE {
                continue;
            }
            seen += 1;
            let site = format!("{}:{}", path.display(), line_of(&text, at));
            let name = path.to_string_lossy().to_string();
            let krate = ["nml-validate", "nml-lsp", "nml-core", "nml-fmt", "nml-cli"]
                .into_iter()
                .find(|c| name.contains(c));
            if let Some(krate) = krate {
                if !crates_seen.iter().any(|s| s == krate) {
                    crates_seen.push(krate.to_string());
                }
            }
            let Some((open, close)) = enclosing_attribute(&clean, at) else {
                findings.push(format!(
                    "{site}: `{FEATURE}` outside any attribute — a `cfg!` branch or a bare \
                     string, either of which can change what the shipped build DOES"
                ));
                continue;
            };
            let attr: String = clean[open..=close]
                .iter()
                .filter(|c| !c.is_whitespace())
                .collect();
            if !ALLOWED.contains(&attr.as_str()) {
                findings.push(format!(
                    "{site}: `{attr}` is not one of the two additive spellings {ALLOWED:?} — a \
                     negated or compound gate makes the feature change behaviour, not add names"
                ));
                continue;
            }
            let word = gated_word(&clean, close + 1);
            if !ITEM_HEADS.contains(&word.as_str()) {
                findings.push(format!(
                    "{site}: the gate sits on `{word}`, not on one of {ITEM_HEADS:?} — a field \
                     or a variant behind the feature reshapes a type under it"
                ));
            }
        }
    }
    assert!(
        seen >= 4,
        "the ratchet found only {seen} `{FEATURE}` gate(s) — it scanned nothing, or the lexer \
         stopped seeing the literal"
    );
    crates_seen.sort();
    assert!(
        crates_seen.iter().any(|c| c == "nml-lsp")
            && crates_seen.iter().any(|c| c == "nml-validate"),
        "both feature-carrying crates must be scanned; saw {crates_seen:?}"
    );
    assert!(
        findings.is_empty(),
        "`{FEATURE}` must only ADD names — a dev-dependency turns it on for the ordinary targets \
         of the same package, so anything it changes is a difference between the binary under \
         test and the binary that ships:\n  {}",
        findings.join("\n  ")
    );
}
