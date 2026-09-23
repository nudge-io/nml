//! The ONE source lexer the tree's RATCHETS read Rust sources through
//! (test-only, feature `test-support`): the CLI's limits census
//! (`limits::tests`) and typography-fold glyph ratchet (`out::tests`),
//! and the editor's `read_dir` ratchet (`wasi_fs.rs`). One lexer, so a
//! `#[cfg(test)] mod` block is excluded the same way by every ratchet —
//! brace-matched, wherever it sits in the file (`main.rs` declares a
//! test module near its top; a ratchet that cut the file there scanned
//! nothing) — and a spelling one ratchet learns (a nested block comment,
//! a `'"'` char literal, a raw string's hash fence) every ratchet
//! knows. Three hand lexers used to live in three crates and each
//! re-learned these on its own. (The kernel's confinement ratchet is a
//! `syn` AST walk — a parser, not a lexer — and stays one.)

use std::path::{Path, PathBuf};

/// The workspace root (two above this crate's manifest directory).
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

/// Every `.rs` source under `dir`, sorted by the caller; `tests`
/// directories are skipped when `want_tests` is false (they hold
/// fixtures, not shipped sentences or bounds) and collected ALONE when
/// it is true.
pub fn sources(dir: &Path, want_tests: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == "node_modules" {
                continue;
            }
            if name == "tests" {
                if want_tests {
                    sources_all(&path, out);
                }
                continue;
            }
            sources(&path, want_tests, out);
        } else if name.ends_with(".rs") && !want_tests {
            out.push(path);
        }
    }
}

/// Every `.rs` file under `dir`, whatever the directory's name.
pub fn sources_all(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources_all(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// ONE pass over a source: line comments, block comments (nested, as
/// Rust nests them), string literals (plain and raw) and char literals
/// are BLANKED — replaced by spaces, one per char, newlines kept, so the
/// result has one char per source char and only an identifier use
/// survives — and every literal's text is handed to `literal` with the
/// char index it starts at. The census reads the blanked text (a bound
/// named in prose or in a JSON-name string is not a pin; a lexer's
/// `'"'` does not swallow the rest of its file), the editor's ratchet
/// reads it for `read_dir` tokens, the glyph ratchet reads the literals.
pub fn lex(text: &str, literal: &mut dyn FnMut(usize, String)) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    while i < chars.len() {
        let c = chars[i];
        // A line comment, to the end of its line.
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        // A block comment, nested as Rust nests them (`/* a /* b */ c */`),
        // to its matching close — newlines kept; an unclosed one runs to
        // the end, as rustc would refuse the file anyway.
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 0usize;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth = depth.saturating_sub(1);
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
            continue;
        }
        // A raw string `r"…"` / `r#"…"#`: closed by `"` + as many `#`.
        if c == 'r' && chars.get(i + 1).is_some_and(|d| *d == '"' || *d == '#') {
            let mut hashes = 0;
            while chars.get(i + 1 + hashes) == Some(&'#') {
                hashes += 1;
            }
            if chars.get(i + 1 + hashes) == Some(&'"') {
                let start = i;
                let mut s = String::new();
                out.push(' ');
                i += 1;
                for _ in 0..=hashes {
                    out.push(' ');
                    i += 1;
                }
                while let Some(&d) = chars.get(i) {
                    if d == '"' && (0..hashes).all(|h| chars.get(i + 1 + h) == Some(&'#')) {
                        out.push(' ');
                        i += 1;
                        for _ in 0..hashes {
                            out.push(' ');
                        }
                        i += hashes;
                        break;
                    }
                    out.push(blank(d));
                    s.push(d);
                    i += 1;
                }
                literal(start, s);
                continue;
            }
        }
        // A plain string, with escapes.
        if c == '"' {
            let start = i;
            let mut s = String::new();
            out.push(' ');
            i += 1;
            let mut escaped = false;
            while let Some(&d) = chars.get(i) {
                out.push(blank(d));
                i += 1;
                if escaped {
                    escaped = false;
                } else if d == '\\' {
                    escaped = true;
                } else if d == '"' {
                    break;
                }
                s.push(d);
            }
            literal(start, s);
            continue;
        }
        // A char literal (`'"'`, `'\''`, `'x'`) — not a lifetime (`'a`).
        if c == '\'' {
            let len = match (chars.get(i + 1), chars.get(i + 2)) {
                // Past the escaped char itself: `'\''` is four chars.
                (Some('\\'), _) => chars[i + 3..]
                    .iter()
                    .position(|d| *d == '\'')
                    .map(|p| p + 4),
                (Some(_), Some('\'')) => Some(3),
                _ => None,
            };
            if let Some(len) = len {
                literal(i, chars[i + 1..i + len - 1].iter().collect());
                for _ in 0..len {
                    out.push(' ');
                }
                i += len;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Line comments, string literals and char literals replaced by spaces
/// (one per char, newlines kept), so only an identifier USE can name a
/// bound.
pub fn blank_comments_and_strings(text: &str) -> String {
    lex(text, &mut |_, _| {})
}

/// The byte ranges, in `clean` (a blanked text), of every
/// `#[cfg(test)]`-attributed `mod` block, brace-matched — from the
/// opening brace to the matching closing one.
pub fn cfg_test_ranges(clean: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(at) = clean[from..].find("#[cfg(test)]") {
        let attr = from + at;
        from = attr + 1;
        let Some(rel) = clean[attr..].find("mod ") else {
            continue;
        };
        let head = &clean[attr..attr + rel];
        // Only an attribute DIRECTLY on a mod (whitespace between).
        if !head[12..].trim().is_empty() {
            continue;
        }
        let Some(open) = clean[attr..].find('{') else {
            continue;
        };
        // `#[cfg(test)] mod tests;` is a FILE module (collected as a
        // `tests/` file): its `;` comes before any brace.
        if clean[attr..].find(';').is_some_and(|semi| semi < open) {
            continue;
        }
        let start = attr + open;
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in clean[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.unwrap_or(clean.len());
        ranges.push((start, end));
        from = end;
    }
    ranges
}

/// The bodies of the `#[cfg(test)] mod` blocks of a blanked text.
pub fn cfg_test_blocks(clean: &str) -> Vec<String> {
    cfg_test_ranges(clean)
        .into_iter()
        .map(|(s, e)| clean[s..e].to_string())
        .collect()
}

/// The string and char literals a PRODUCT sentence can carry: every
/// literal of `text` outside its comments and its `#[cfg(test)] mod`
/// blocks, wherever those sit in the file.
pub fn product_literals(text: &str) -> Vec<String> {
    let mut literals: Vec<(usize, String)> = Vec::new();
    let clean = lex(text, &mut |at, s| literals.push((at, s)));
    // The blanked text has one char per source char, so a byte range of
    // it maps to a char range of the source by counting chars.
    let ranges: Vec<(usize, usize)> = cfg_test_ranges(&clean)
        .into_iter()
        .map(|(s, e)| (clean[..s].chars().count(), clean[..e].chars().count()))
        .collect();
    literals
        .into_iter()
        .filter(|(at, _)| !ranges.iter().any(|(s, e)| (*s..*e).contains(at)))
        .map(|(_, s)| s)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{blank_comments_and_strings, cfg_test_blocks, product_literals, workspace};

    /// ONE lexer serves every source ratchet — the CLI's limits census
    /// and glyph ratchet, the editor's `read_dir` ratchet: a second hand
    /// lexer re-learns each spelling on its own and drifts from the
    /// others. The three names the deleted ones were spelled with are
    /// forbidden in EVERY file that reads the shared lexer, not one name
    /// per file: a per-file needle is vacuous wherever the name it names
    /// was never in that file. The kernel's confinement ratchet is a
    /// `syn` AST walk — a parser, the right tool there — and is not a
    /// lexer to fold.
    #[test]
    fn one_lexer_serves_every_ratchet() {
        let ws = workspace();
        assert!(
            ws.join("Cargo.toml").exists(),
            "the workspace root: {}",
            ws.display()
        );
        assert!(
            !ws.join("nml-cli/src/scan.rs").exists(),
            "a second lexer in the CLI"
        );
        for file in [
            "nml-cli/src/limits.rs",
            "nml-cli/src/out.rs",
            "crates/nml-lsp/src/wasi_fs.rs",
        ] {
            let text = std::fs::read_to_string(ws.join(file)).expect(file);
            for own in ["fn lex(", "fn code_only(", "fn blank_comments"] {
                assert!(!text.contains(own), "{file}: a second lexer ({own})");
            }
            assert!(
                text.contains("test_support::scan"),
                "{file}: does not read through the shared lexer"
            );
        }
    }

    /// Block comments are blanked as comments, nested as Rust nests
    /// them, and a `read_dir` inside one — or inside a string, a raw
    /// string or a `'"'` char literal's wake — is no token; outside
    /// them it is (the editor's ratchet's bypass shapes, on the one
    /// lexer).
    #[test]
    fn block_comments_and_literals_are_blanked_and_tokens_survive() {
        let tokens =
            |src: &str| -> String { blank_comments_and_strings(src).split_whitespace().collect() };
        for src in [
            "std::fs::read_dir(&d)",
            "d.read_dir()",
            "use std::fs::{read_dir as rd};",
            "let p = base.join(\"a//b\"); std::fs::read_dir(&p)",
            "let c = '\"'; std::fs::read_dir(&d)",
            "/* opened */ std::fs::read_dir(&d) /* closed */",
        ] {
            assert!(tokens(src).contains("read_dir"), "a token: {src}");
        }
        for src in [
            "// std::fs::read_dir(&d) in a comment",
            "/* fs::read_dir in a block /* nested */ comment */",
            "/* unclosed: read_dir",
            "let s = \"read_dir mentioned in a string\";",
            "let r = r#\"read_dir in a raw string\"#;",
        ] {
            assert!(!tokens(src).contains("read_dir"), "no token: {src}");
        }
        let src = "a /* x\ny */ b";
        let clean = blank_comments_and_strings(src);
        assert_eq!(clean.chars().count(), src.chars().count());
        assert_eq!(clean.lines().count(), 2, "newlines kept: {clean:?}");
    }

    /// The one lexer, executed: the blanked view keeps one char per
    /// source char and only identifiers; the literal view keeps every
    /// literal's text; a test module is excluded wherever it sits, and
    /// a literal after an EARLY test module is still a product literal.
    #[test]
    fn the_shared_lexer_blanks_and_collects_the_same_literals() {
        let src = "#[cfg(test)]\nmod early { const X: &str = \"in-test — glyph\"; }\n\
                   fn f() { let a = \"prod — a\"; let b = r#\"raw \"x\" … b\"#; let c = '—'; \
                   let d = '\\''; } // \"comment — no\"\n\
                   #[cfg(test)]\nmod tests { fn g() { let z = \"late — z\"; } }\n";
        let clean = blank_comments_and_strings(src);
        assert_eq!(clean.chars().count(), src.chars().count());
        assert!(!clean.contains('—') && !clean.contains("prod") && clean.contains("let a ="));
        assert_eq!(cfg_test_blocks(&clean).len(), 2);
        let lits = product_literals(src);
        assert_eq!(lits, ["prod — a", "raw \"x\" … b", "—", "\\'"]);
    }
}
