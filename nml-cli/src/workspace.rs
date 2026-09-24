//! The WORKSPACE a run resolves in — [`Workspace`]: the root, the one
//! universe walked under it, every argument classified against it and
//! expanded to files, the leaves opened beneath it — shared by `nml
//! check`, `nml validate`, `nml fix` and `nml binding` (RFC 0017 §4.1,
//! RFC 0019 item 0). A verb that assembled a different universe from
//! another's would judge files differently than CI does — so the
//! assembly exists once, here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nml_core::diagnostic::{Diagnostic, codes};
use nml_validate::fs::{EntryKind, MAX_SOURCE_BYTES, ReadError, StdFs, read_beneath};
use nml_validate::schema::SchemaValidator;
use nml_validate::workspace::{
    AuditBudget, ClaimOrigin, Discovery, Endpoint, ExternalClaim, ExternalClass, Governing,
    InputKind, PathError, Resolved, RootOrigin, Shadow, Skip, SourceKey, Truncation, Trust,
    Universe, WorkspaceRoot, ambiguous_claim_summary, audit_hidden, audit_incomplete, discover,
    path_finding_typed, read_input, resolve_file, skipped as skipped_row, skipped_under,
};

/// A `--schema` directory the run cannot read IN FULL is the INVOCATION's
/// mistake (an unset variable, a typo, a permission): a usage error,
/// exit 2, in every verb that takes the flag, checked ONCE before any
/// target runs — the directory, every entry and every schema source
/// ([`read_schema_dir`]). `check` used to fail per target with exit 1,
/// `fix` degraded to the file alone with exit 0, and an entry whose
/// kind could not be read was `flatten()`ed away: a schema universe
/// that silently shrank, under a green gate.
pub fn require_schema_dir(dir: &Path) -> Result<(), String> {
    let sources = read_schema_dir(dir)?;
    // The run's own fact, recorded once, before any target: how many schema
    // sources the invocation's directory contributed. It rides the closing
    // row for a consumer that reads no prose.
    crate::out::set_schema_sources(sources.len());
    // NONE is the case worth saying. A directory with no source is a
    // LEGITIMATE invocation — RFC 0012's self-validating file carries its
    // own `model`, and the formatter's own fixture runs pass `--schema`
    // over directories of instances — so it is disclosed, never refused:
    // the run proceeds, its exit code untouched, and the operator is told
    // what the directory contributed rather than reading `ok` from a run
    // that loaded nothing. `--quiet` is errors only, so it stays silent;
    // `--json` says the same thing as a number.
    if sources.is_empty() && !crate::out::json() && !crate::out::quiet() {
        crate::out::err(format_args!(
            "{} --schema {} holds no schema source ({}): every target is validated \
             against its own definitions only",
            crate::out::paint(crate::out::Level::Note, "note:"),
            crate::sanitized(&display_path(dir)),
            nml_validate::workspace::SCHEMA_SOURCE_SUFFIXES.join(", ")
        ));
    }
    Ok(())
}

/// Read a schema directory's sources (`*.model.nml` / `*.schema.nml`),
/// listed through the kernel's ONE listing rule (`fs::listing`: an
/// entry whose kind cannot be read refuses the whole listing, sorted)
/// and read whole. Every failure is the invocation's — a usage error in
/// the tool's own words, never `io::Error`'s `(os error N)` tail (the
/// kernel's `FsError` spells an entry's). Loading happens once, in the
/// caller's single schema universe (RFC 0012); parse errors surface
/// later as attributed diagnostics. A link is read by path, as
/// `rustfmt` reads one: an operator's invocation input, not a
/// workspace file. Every source is OPENED as a target is
/// ([`read_schema_source`]): a FIFO, socket or device so named is
/// refused, never blocked on.
pub fn read_schema_dir(dir: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let opened = std::fs::read_dir(dir)
        .map_err(|e| schema_dir_error(dir, &io_reason(&e, "cannot read the directory")))?;
    schema_sources_of(nml_validate::fs::listing(Ok(opened)), dir)?
        .into_iter()
        .map(|path| {
            read_schema_source(&path)
                .map(|text| (path.clone(), text))
                .map_err(|why| {
                    schema_dir_error(dir, &format!("cannot read {}: {why}", message_path(&path)))
                })
        })
        .collect()
}

/// One `--schema` source, read as [`Workspace::open_target`] reads a
/// target in an open universe: the leaf resolved ONCE by path
/// (`leaf_under_parent` — a link is followed, an operator's invocation
/// input), then the kernel's ONE reader at that leaf ([`read_beneath`]
/// — unix: `O_NOFOLLOW | O_NONBLOCK`, `fstat` must say regular file),
/// so a FIFO, socket or device named `*.model.nml` is refused in the
/// tool's words — the by-path `read_to_string` blocked the run forever
/// on one, a hang under a green gate — and a directory so named is the
/// invocation's mistake. The bytes are bounded by the SAME
/// [`MAX_SOURCE_BYTES`] a manifest-declared source is read under: a
/// schema file reached through `--schema` and the same file reached
/// through a manifest are refused at the same size, in the same
/// sentence, so no path into the tool reads a source the other would
/// refuse. The reason never carries `io::Error`'s `(os error N)` tail.
fn read_schema_source(path: &Path) -> Result<String, String> {
    use nml_validate::fs::OpenError;
    let at = crate::leaf_under_parent(path)?;
    read_beneath(
        &at.dir,
        &[at.name.as_str()],
        MAX_SOURCE_BYTES,
        "a schema source",
    )
    .map_err(|e| match e {
        ReadError::Open(OpenError::NotRegular { dir: true, .. }) => "is a directory".to_string(),
        ReadError::Open(OpenError::NotRegular { dir: false, .. }) => {
            "not a regular file — a FIFO, socket or device is never opened".to_string()
        }
        ReadError::Open(e @ OpenError::Symlink { .. }) => crate::leaf_advice(e),
        ReadError::Open(e) => io_reason(&e.into_io(), "cannot read the file"),
        // The cap's one sentence and a non-UTF-8 refusal are the
        // kernel's, spoken identically by every front end.
        e => e.to_string(),
    })
}

/// The schema-named entries of a `--schema` listing, in the kernel's
/// (sorted) order — pure over the listing, so the refusal of an
/// unreadable ENTRY is pinned without a filesystem that fails per
/// entry: a refused listing is the invocation's mistake.
fn schema_sources_of(
    listing: nml_validate::fs::Listing,
    dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let entries =
        listing.map_err(|e| schema_dir_error(dir, &format!("cannot list the directory: {e}")))?;
    Ok(entries
        .into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(nml_validate::workspace::is_schema_source_name)
        })
        .map(|(name, _)| dir.join(name))
        .collect())
}

/// `--schema <dir>: <why>`, marked as the invocation's mistake (exit 2).
fn schema_dir_error(dir: &Path, why: &str) -> String {
    crate::out::usage_error(format!("--schema {}: {why}", message_path(dir)))
}

/// An I/O failure's reason in the tool's own words — never `io::Error`'s
/// `(os error N)` tail, which no other refusal carries; `other` names the
/// act for a kind the vocabulary has no word for.
fn io_reason(e: &std::io::Error, other: &str) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such directory".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        std::io::ErrorKind::NotADirectory => "not a directory".to_string(),
        std::io::ErrorKind::IsADirectory => "is a directory".to_string(),
        kind => format!("{other} ({kind})"),
    }
}

/// The tree-wide READ ratchet: every byte a product source reads is
/// bounded — through the crate's ONE capped reader, or by a `Take`
/// adaptor, and by nothing else.
///
/// A byte read spelled any other way is UNBOUNDED by construction:
/// `read_to_string` grows a `String` to whatever the file is, and
/// `read_line` to whatever arrives before a newline. Four such reads
/// stood in this tree at once and each was found by hand, one
/// certification round apart — the `--schema` source (this file), a
/// store slot's manifest and declared sources (`package::from_dir`),
/// the store's `current` pointer, and the editor transport's HEADER
/// lines, the last of them past a bound (`MAX_FRAME_BYTES`) that only
/// ever capped the body. Finding the class by hand is what failed;
/// this finds it at unit-test speed, in every crate, on every build.
///
/// The matcher is deliberately maximal, like the editor's `read_dir`
/// ratchet: over PRODUCT text only (comments, string literals and
/// brace-matched `#[cfg(test)] mod` blocks blanked by the one shared
/// lexer — a test reads its own scratch and must), ANY remaining token
/// from [`READ_TOKENS`] is a violation, whatever the spelling — the free
/// form (`fs::read_to_string(`), the method form (`f.read_to_end(`),
/// UFCS (`Read::read_to_string(`), an import, an `as` alias, a
/// function-pointer binding.
///
/// Two spellings pass, and they are the two ways a read CAN be bounded:
///
/// * the kernel's readers (`read_beneath`, `read_leaf`, `read_input`)
///   carry no token of the list in their names, so routing a read
///   through one IS how a file passes — that is the rule, not an
///   exemption;
/// * a `Take` adaptor immediately before the read
///   (`r.take(n).read_to_end(..)`) bounds it by construction — the one
///   spelling [`ALLOWED`] therefore does not have to name a file for.
///   The capped reader itself is one, and so is the transport's header
///   line.
///
/// [`ALLOWED`] is per (file, TOKEN), never per file, so a file excused
/// for one read is not excused for a different one added later.
#[cfg(test)]
mod read_ratchet {
    use nml_validate::test_support::scan::{
        blank_comments_and_strings, cfg_test_ranges, sources, workspace,
    };

    /// Every spelling of an UNBOUNDED byte read in `std`. `read_dir` is
    /// NOT here: a listing is a different class with its own rule
    /// (`fs::listing`) and its own ratchet (`wasi_fs.rs`).
    const READ_TOKENS: &[&str] = &[
        "read_to_string",
        "read_to_end",
        "read_exact",
        "read_line",
        "read_until",
        "fs::read(",
    ];

    /// The reads a `Take` adaptor bounds by construction, as method
    /// calls on it.
    const BOUNDED_BY_TAKE: &[&str] = &["read_to_string", "read_to_end", "read_line", "read_until"];

    /// (file, token, why) — a read that is bounded by something other
    /// than a `Take`. Each file is checked to still exist and each row
    /// to still be NEEDED, so a row cannot outlive the code it excuses.
    const ALLOWED: &[(&str, &str, &str)] = &[(
        "crates/nml-lsp/src/transport/framing.rs",
        "read_exact",
        "the JSON-RPC body: the buffer is sized from a `Content-Length` already refused past \
         `MAX_FRAME_BYTES`, so the read is bounded by the buffer",
    )];

    /// The product text of a source: the one shared lexer's blanking,
    /// with every `#[cfg(test)] mod` block blanked too.
    fn product_text(text: &str) -> String {
        let clean = blank_comments_and_strings(text);
        let mut out: Vec<char> = clean.chars().collect();
        let len = out.len();
        // Ranges are byte offsets into `clean`, which has one char per
        // source char; count chars to index the char vector.
        for (s, e) in cfg_test_ranges(&clean) {
            let (s, e) = (clean[..s].chars().count(), clean[..e].chars().count());
            for c in out.iter_mut().take(e.min(len)).skip(s) {
                if *c != '\n' {
                    *c = ' ';
                }
            }
        }
        out.into_iter().collect()
    }

    /// Whitespace-free code, so every spelling of one call collapses to
    /// one substring.
    fn collapse(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join("")
    }

    /// Blank the read token of every `take`-bounded read in a collapsed
    /// text — `r.take(n).read_to_end(..)` — matching the adaptor's
    /// parentheses, so the bound may be any expression.
    fn scrub_bounded_reads(collapsed: &str) -> String {
        const TAKE: &[char] = &['.', 't', 'a', 'k', 'e', '('];
        let chars: Vec<char> = collapsed.chars().collect();
        let mut out = chars.clone();
        for i in 0..chars.len() {
            if !chars[i..].starts_with(TAKE) {
                continue;
            }
            let mut depth = 0usize;
            let mut j = i + 5;
            while j < chars.len() {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if j >= chars.len() {
                continue;
            }
            let rest: String = chars[j + 1..].iter().collect();
            for token in BOUNDED_BY_TAKE {
                if rest.starts_with(&format!(".{token}(")) {
                    for k in 0..token.len() {
                        out[j + 2 + k] = '\u{0}';
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// The tokens a collapsed, scrubbed text still names.
    fn unbounded_reads(code: &str) -> Vec<&'static str> {
        let scrubbed = scrub_bounded_reads(&collapse(code));
        READ_TOKENS
            .iter()
            .filter(|t| scrubbed.contains(**t))
            .copied()
            .collect()
    }

    /// The tree's product sources, as (repo-relative path, text).
    fn product_sources() -> Vec<(String, String)> {
        let root = workspace();
        let mut files = Vec::new();
        sources(&root.join("crates"), false, &mut files);
        sources(&root.join("nml-cli").join("src"), false, &mut files);
        files.sort();
        files
            .iter()
            .filter_map(|f| {
                let text = std::fs::read_to_string(f).ok()?;
                let rel = f
                    .strip_prefix(&root)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                Some((rel, text))
            })
            .collect()
    }

    #[test]
    fn every_byte_read_in_the_tree_is_bounded() {
        let root = workspace();
        for (file, token, why) in ALLOWED {
            assert!(
                root.join(file).is_file(),
                "the read allow-list names {file}, which is gone — drop the row ({why})"
            );
            assert!(
                READ_TOKENS.contains(token),
                "{file}: {token} is not a read token the ratchet looks for"
            );
        }
        let mut offenders = Vec::new();
        let mut used: Vec<(&str, &str)> = Vec::new();
        let mut scanned: Vec<String> = Vec::new();
        for (rel, text) in product_sources() {
            scanned.push(rel.clone());
            let product = product_text(&text);
            let excused: Vec<&str> = ALLOWED
                .iter()
                .filter(|(f, _, _)| *f == rel)
                .map(|(_, t, _)| *t)
                .collect();
            // The WHOLE FILE is the authority: a `take`-bounded read can
            // be spelled across lines, so a line read alone would read as
            // unbounded. Lines only LOCATE what the file already names.
            let found = unbounded_reads(&product);
            for token in &found {
                if excused.contains(token) {
                    used.push((
                        ALLOWED
                            .iter()
                            .find(|(f, t, _)| *f == rel && t == token)
                            .expect("row")
                            .0,
                        *token,
                    ));
                }
            }
            let unexcused: Vec<&str> = found
                .iter()
                .filter(|t| !excused.contains(*t))
                .copied()
                .collect();
            if unexcused.is_empty() {
                continue;
            }
            for (i, (line, clean)) in text.lines().zip(product.lines()).enumerate() {
                let code = collapse(clean);
                if unexcused.iter().any(|t| code.contains(t)) {
                    offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        // Anti-vacuity: one source of every crate the rule covers must
        // be in the scan, by name — a walk that found nothing (a moved
        // root, a `CARGO_MANIFEST_DIR` baked at another tree) would
        // otherwise pass silently.
        for must in [
            "crates/nml-core/src/lib.rs",
            "crates/nml-validate/src/fs/mod.rs",
            "crates/nml-validate/src/package.rs",
            "crates/nml-validate/src/store.rs",
            "crates/nml-fmt/src/formatter.rs",
            "crates/nml-lsp/src/lib.rs",
            "crates/nml-lsp/src/server.rs",
            "nml-cli/src/workspace.rs",
        ] {
            assert!(
                scanned.iter().any(|s| s == must),
                "the ratchet did not scan {must} — it scanned {} sources: {scanned:?}",
                scanned.len()
            );
        }
        assert!(
            offenders.is_empty(),
            "an UNBOUNDED read ({} product sources scanned). Route it through \
             `nml_validate::fs::{{read_beneath, read_leaf}}` or `workspace::read_input` under a \
             published \
             bound, or `.take(bound)` it, or add (file, token, why) to ALLOWED:\n{}",
            scanned.len(),
            offenders.join("\n")
        );
        for (file, token, why) in ALLOWED {
            assert!(
                used.contains(&(*file, *token)),
                "the allow-list row ({file}, {token}) excuses nothing any more — drop it ({why})"
            );
        }
    }

    /// The matcher itself: every bypass class it must catch, and the
    /// masking shapes it must NOT be fooled by (a mention in a comment,
    /// a string, a raw string, a test module, a `take`-bounded read).
    #[test]
    fn the_read_matcher_catches_every_bypass_shape() {
        let catches = [
            "fn f() { let t = std::fs::read_to_string(p); }",
            "fn f() { let b = std::fs::read(p); }",
            "fn f() { let b = s::fs::read(p); }",
            "fn f() { f.read_to_end(&mut v); }",
            "fn f() { std::io::Read::read_to_string(&mut f, &mut s); }",
            "use std::fs::read_to_string;",
            "use std::fs::{read_to_string as slurp};",
            "fn f() { let g = std::fs::read_to_string; g(p); }",
            "fn f() { r.read_exact(&mut b); }",
            "fn f() { r.read_line(&mut l); }",
            "fn f() { r.read_until(b'\\n', &mut v); }",
            "fn f() { let c = '\"'; let t = std::fs::read_to_string(p); }",
            "fn f() { let t = std :: fs :: read_to_string (p); }",
            "fn f() { let t = std::fs::read_to_string/* */(p); }",
            // A `take` that does not bound THIS read.
            "fn f() { let n = v.iter().take(2).count(); r.read_to_end(&mut b); }",
        ];
        for src in catches {
            assert!(
                !unbounded_reads(&product_text(src)).is_empty(),
                "must catch: {src}"
            );
        }
        let passes = [
            "// std::fs::read_to_string(p) in a comment",
            "/* fs::read_to_end /* nested */ */",
            "fn f() { let s = \"read_to_string mentioned in a string\"; }",
            "fn f() { let r = r#\"read_line in a raw string\"#; }",
            "#[cfg(test)] mod tests { fn t() { let x = std::fs::read_to_string(p); } }",
            "fn f() { let t = read_beneath(root, &k, CAP, \"a target\"); }",
            "fn f() { let t = read_leaf(p, CAP, \"a file\"); }",
            "fn f() { let t = read_input(&root, kind, p); }",
            "fn f() { let e = std::fs::read_dir(p); }",
            // Bounded by construction, however the bound is spelled.
            "fn f() { r.take(cap as u64 + 1).read_to_end(&mut b); }",
            "fn f() { r.by_ref().take(MAX_HEADER_BYTES).read_line(&mut l); }",
            "fn f() { r.take(f(g(1), 2)).read_to_string(&mut s); }",
        ];
        for src in passes {
            assert_eq!(
                unbounded_reads(&product_text(src)),
                Vec::<&str>::new(),
                "must pass: {src}"
            );
        }
    }
}

#[cfg(test)]
mod schema_dir_tests {
    use super::*;
    use nml_validate::fs::FsError;
    use std::ffi::OsString;

    /// The `--schema` listing goes through the kernel's rule: a listing
    /// the kernel refused (one entry's kind unreadable — `Denied`) is
    /// the invocation's mistake, never a smaller schema universe; a
    /// readable one yields the schema-named entries in the kernel's
    /// order, whatever their kind (a link is read by path; a directory
    /// so named fails its read as the invocation's mistake too).
    #[test]
    fn an_unreadable_schema_entry_refuses_the_invocation_never_shrinks_the_universe() {
        let dir = Path::new("schemas");
        let refused = schema_sources_of(Err(FsError::Denied), dir).expect_err("refused");
        assert_eq!(
            refused,
            "--schema schemas: cannot list the directory: permission denied on a path component"
        );
        assert!(crate::out::is_usage(), "the invocation's mistake: exit 2");
        let listing = Ok(vec![
            (OsString::from("a.schema.nml"), EntryKind::Symlink),
            (OsString::from("b.model.nml"), EntryKind::File),
            (OsString::from("c.model.nml"), EntryKind::Dir),
            (OsString::from("notes.txt"), EntryKind::File),
            (OsString::from("x.nml"), EntryKind::File),
        ]);
        let names: Vec<String> = schema_sources_of(listing, dir)
            .expect("listed")
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            names,
            [
                "schemas/a.schema.nml",
                "schemas/b.model.nml",
                "schemas/c.model.nml"
            ]
        );
    }
}

/// One schema universe per check (RFC 0012): the `--schema` directory's
/// sources plus the checked file itself — unless it *is* one of them
/// (path-canonicalized, so the same file reached two ways is never loaded
/// twice and never reported as its own duplicate). Each entry is
/// `(load_name, path, text)`: the checked file's load name is its KEY
/// (`own_name`: its KEY — step 0f); a `--schema` directory's source
/// is the operator's invocation input, not a workspace file, and is
/// named by its basename — unambiguous against a key, which always
/// spells a path under the root.
pub fn schema_universe(
    path: &Path,
    own_name: &str,
    source: &str,
    schema_dir: Option<&PathBuf>,
) -> Result<Vec<(String, PathBuf, String)>, String> {
    let mut named_sources: Vec<(String, PathBuf, String)> = Vec::new();
    if let Some(sd) = schema_dir {
        for (p, text) in read_schema_dir(sd)? {
            let load_name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("schema")
                .to_string();
            named_sources.push((load_name, p, text));
        }
    }
    let file_canon = path.canonicalize().ok();
    let mut file_is_a_source = false;
    for (_, p, text) in &mut named_sources {
        if file_canon.is_some() && p.canonicalize().ok() == file_canon {
            file_is_a_source = true;
            // The caller's text wins over the disk copy: `nml fix` analyzes
            // in-memory candidates mid-round, and the universe must see the
            // same bytes the file-level analysis sees.
            *text = source.to_string();
        }
    }
    if !file_is_a_source {
        named_sources.push((own_name.to_string(), path.to_path_buf(), source.to_string()));
    }
    Ok(named_sources)
}

/// The schema universe as the loader's parts, with the checked file's
/// slot served from ITS OWN extraction. `check` and `fix` have
/// already parsed the target once (`parse_and_extract_split`); the slot
/// whose text IS the target's text — the file appended by
/// [`schema_universe`], or the `--schema` directory entry it happens to
/// be — takes that extraction and its facet findings under the load name
/// the universe gave it. Every other source is extracted here, one at a
/// time, exactly as `load_schema` itself would; the loader's findings,
/// order and attribution are unchanged. Shared by both verbs so the
/// fixer can never judge a file differently than `check` does.
pub fn schema_parts<'a>(
    named_sources: &'a [(String, PathBuf, String)],
    source: &'a str,
    own: (
        nml_core::schema::ExtractedSchema,
        Vec<nml_core::diagnostic::Diagnostic>,
    ),
) -> impl Iterator<Item = nml_validate::loader::SchemaPart<'a>> + 'a {
    let mut own = Some(own);
    named_sources.iter().map(move |(n, _, t)| {
        if t.as_str() == source {
            if let Some((schema, diags)) = own.take() {
                return (n.as_str(), schema, diags);
            }
        }
        let (extracted, errors) = nml_core::cst::extract_schema(t);
        (n.as_str(), extracted, errors)
    })
}

/// The invocation's universe (RFC 0019 item 0, step 0d): the workspace
/// root fixed ONCE per invocation — `--root`, else derived from the
/// invocation target within its VCS fence (E21) — and the bounded
/// discovery under it. The builtin meta package is the one external
/// claim the CLI knows (it governs `*.package.nml`, exactly as in the
/// editor). Shared by `check`, `fix` and `binding`, so no verb can judge
/// a file under a different universe than another.
pub struct Workspace {
    discovery: Discovery,
    /// Every argument of the invocation, AS TYPED, with the kernel's
    /// classification of it against the root — settled in [`Self::open`]
    /// BEFORE the walk, read by [`Self::expand_targets`]: one
    /// classification per argument, and a target outside the root
    /// refused before any universe is built.
    classified: Vec<(String, Result<Endpoint, PathError>)>,
}

/// The byte bound on the check TARGET itself: the one
/// tenant-committed input the discovery caps
/// ([`nml_validate::workspace::input_cap`], the kernel's per-kind policy — 256 KiB for a manifest or a project
/// config, 4 MiB for a declared schema source) did not cover. The caps
/// are POLICY bounds on what an input may reasonably be, not a parser
/// cost (the parser is linear): a target is one document, and 16 MiB
/// is far past any configuration file while still bounding the memory
/// a `check`/`validate`/`fix` over a hostile tree can be made to hold
/// (a target costs a large multiple of its size in AST and diagnostics).
/// Read with the same `take(cap + 1)` discipline as the discovery reads
/// — an oversized target is refused without being read in, naming the
/// cap. A `--schema` directory's sources stay uncapped: the operator's
/// own invocation input, never content a tenant commits.
///
/// LIMIT: reach=content guards=memory surface=cli shown="16 MiB" — bytes of a check/validate/fix TARGET
pub const MAX_TARGET_BYTES: usize = 16 * 1024 * 1024;

/// A path as a human standing in the working directory spells it — the
/// CLI's OWN lines only (the `note:`, `binding`'s `root` line, the
/// outside-root refusal, the tag's shadowing entry, marker and `--root`
/// advice), never a kernel sentence, which is one text for both front
/// ends: `.` at the working directory, `..`/`../..` above it, `tenants/cu`
/// below it (git's `status.relativePaths`), and the canonical path when
/// neither contains the other — as it stands on every wire row. Both
/// sides are physical paths (`current_dir` is `getcwd`; every path here
/// is canonical), so the comparison never crosses a link.
pub(crate) fn display_path(path: &Path) -> String {
    forward_slashes(display_path_inner(path))
}

fn forward_slashes(s: String) -> String {
    s.replace('\\', "/")
}

/// Windows extended-path spellings (`\\?\`, `\\?\UNC\`) are for syscalls;
/// human lines match paths the operator or tempdir APIs spell without them.
fn path_for_display(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn display_path_inner(path: &Path) -> String {
    let path = path_for_display(path);
    let Ok(cwd) = std::env::current_dir().map(|c| path_for_display(&c)) else {
        return forward_slashes(path.display().to_string());
    };
    if path == cwd {
        return ".".to_string();
    }
    if let Ok(below) = path.strip_prefix(&cwd) {
        return forward_slashes(below.display().to_string());
    }
    match cwd.strip_prefix(&path) {
        Ok(above) => {
            let ups: PathBuf = above.components().map(|_| "..").collect();
            forward_slashes(ups.display().to_string())
        }
        Err(_) => forward_slashes(path.display().to_string()),
    }
}

/// Paths in kernel-aligned prose (errors, schema refusals): forward
/// slashes on every platform, like source keys.
pub(crate) fn message_path(path: &Path) -> String {
    forward_slashes(path.display().to_string())
}

/// The root path on `--json` rows — `canonicalize`'s spelling on each OS
/// (`\\?\` on Windows when the API returns it).
pub(crate) fn wire_root_path(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Absolutize a path against the current directory: the kernel has no
/// ambient authority, so the working directory is joined HERE, once.
pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| format!("cannot determine the current directory: {e}"))
}

impl Workspace {
    pub fn open(root_flag: Option<&Path>, targets: &[String]) -> Result<Self, String> {
        let Some(first) = targets.first() else {
            return Err(crate::out::usage_error("a target is required".to_string()));
        };
        let target_arg = Path::new(first);
        let target = absolute(target_arg)?;
        let root = match root_flag {
            // A `--root` that is no directory is the INVOCATION's
            // mistake: a usage error, exit 2.
            Some(dir) => WorkspaceRoot::explicit(&absolute(dir)?, &StdFs)
                .map_err(|e| crate::out::usage_error(format!("--root {}: {e}", dir.display())))?,
            None => WorkspaceRoot::derive(&target, &StdFs).map_err(|e| match e {
                // Closed-denied derivations: a root marker above a
                // planted or submodule `.git` (the shadow check) and the
                // walk bound met with no VCS root — the INVOCATION's to
                // settle by naming the root, so a usage error, exit 2.
                e @ (nml_validate::workspace::RootError::Shadowed { .. }
                | nml_validate::workspace::RootError::ShadowUnchecked { .. }
                | nml_validate::workspace::RootError::ComponentCap { .. }) => {
                    // The kernel's sentence with its paths spelled from the
                    // working directory, as every other line of this front
                    // end spells them, the target as typed; for a marker
                    // above the fence, the concrete `--root` that checks
                    // under that manifest's universe (its directory,
                    // spelled — the advice `origin_tag` gives a shadow).
                    let alternative = match &e {
                        nml_validate::workspace::RootError::Shadowed { marker, .. } => format!(
                            "; --root {} checks under that manifest",
                            marker.parent().map(display_path).unwrap_or_default()
                        ),
                        _ => String::new(),
                    };
                    crate::out::usage_error(format!(
                        "cannot derive a workspace root for {}: {} (pass --root <dir>{alternative})",
                        target_arg.display(),
                        e.display_with(&display_path)
                    ))
                }
                // The target's directory is absent or not a directory
                // (the kernel kind-checks every resolved endpoint), so
                // the target cannot exist: say that, and which flag
                // names a universe — the flag names the universe, it
                // does not make the file exist, so the sentence says
                // which is which rather than selling `--root` as a fix
                // for a mistyped path.
                nml_validate::workspace::RootError::NotADirectory => format!(
                    "{}: no such file or directory (its directory does not exist either — check \
                     the path; a workspace root is derived from the target's directory, so \
                     pass --root <dir> to name one instead)",
                    target_arg.display()
                ),
                e => format!(
                    "cannot derive a workspace root for {}: {} (pass --root <dir>)",
                    target_arg.display(),
                    e.display_with(&display_path)
                ),
            })?,
        };
        // The root is the run's FIRST fact — on the closing row before
        // any argument is judged — so a run refused at its invocation
        // still names the universe it was refused against.
        crate::out::set_root(root_facts_of(&root));
        // Every argument is classified against the root BEFORE the walk
        // runs (clig.dev: validate early): a target outside the root is
        // the invocation's mistake, and a wrong invocation is refused
        // before the universe is built — no walk, no universe error,
        // nothing listed, exit 2. The classification is the kernel's own
        // (`SourceKey::classify` under closed trust whatever the universe
        // — E31 option (a), the certified behaviour: below the root it
        // never follows a link, the universe walk judges it) — the SAME
        // one the expansion reads, never a lexical rule: a link followed
        // by `..` is a halt to the walk (NML2083) and an escape to a
        // lexer, and the walk's verdict is the one this front end gives.
        // Classified ONCE, here; [`Self::expand_targets`] reads these
        // verdicts.
        let mut classified = Vec::with_capacity(targets.len());
        for arg in targets {
            let abs = absolute(Path::new(arg))?;
            let verdict = SourceKey::classify(&root, &abs, &StdFs, Trust::Closed);
            if let Err(PathError::Escapes { .. }) = &verdict {
                return Err(outside_root_of(&root, Path::new(arg)));
            }
            classified.push((arg.clone(), verdict));
        }
        let builtin = ExternalClaim::new(
            Arc::new(nml_validate::package::builtin_meta_package()),
            ExternalClass::Builtin,
        );
        // The disk case of the kernel's `ReadText`, the kernel's own
        // (`read_input`: the race-free chain under the root, the kind's
        // cap, one sentence) — the same call the editor makes.
        let read = |kind: InputKind, path: &Path| read_input(&root, kind, path);
        let discovery = discover(&root, &StdFs, &read, vec![builtin], Arc::default());
        let ws = Self {
            discovery,
            classified,
        };
        // The run-scoped universe facts, recorded ONCE for the closing
        // row: the root, HOW it was fixed
        // (`RootOrigin::tag` — the kernel's stable spelling, the same
        // one the `binding` row carries), whether the universe is closed
        // and how many manifests it discovered. Before this a consumer
        // had to run `nml binding` as a second process to learn which
        // universe `nml check` had just judged its file in. The budget
        // units the walk stopped inside (the A16 amendment)
        // ride the same row: a unit's exhaustion leaves the universe
        // closed and its manifests counted, so without them the row
        // would read as a whole universe. The kernel's `Closure` rides
        // it too: `closed` alone would conflate a complete universe
        // with a truncated or an unloadable one.
        let universe = ws.discovery.universe();
        crate::out::set_universe(
            universe.state(),
            universe.closure.tag(),
            universe.workspace_claims(),
            truncated_units(&universe),
            ws.discovery
                .skipped()
                .iter()
                .map(|s| {
                    (
                        s.key.to_string(),
                        s.why.tag(),
                        s.entry().map(str::to_string),
                    )
                })
                .collect(),
        );
        // The two derived shapes an operator could not see in human mode
        // (the JSON root object and the `binding` line always said so):
        // a fence entry that is not a directory — a linked worktree's, a
        // submodule's or a planted `.git` file — and a universe shadowed
        // by another `.git` above its fence. Disclosed once, on stderr,
        // before any finding; `--quiet` keeps errors only.
        if ws.discovery.root().origin().needs_disclosure()
            && !crate::out::json()
            && !crate::out::quiet()
        {
            crate::out::err(format_args!(
                "{} workspace root {}  {}",
                crate::out::paint(crate::out::Level::Note, "note:"),
                crate::sanitized(&display_path(ws.discovery.root().path())),
                ws.origin_tag()
            ));
        }
        Ok(ws)
    }

    /// The gate's rows over the directories named on the command line
    /// (the fail-closed gate): the `.nml` content the walk left out of its
    /// enumeration by policy under any of `dirs`, as findings — a
    /// symlinked `.nml` (NML2083 in a closed universe, in the resolver's
    /// words; NML2090 in an open one, where a link is followed only when
    /// named), a `.nml` FIFO, a `.nml` dot-file, every `.nml` file or
    /// link a skipped dot-directory holds (an audit bounded by ONE
    /// [`AuditBudget`] per run, `.git` excepted), a hidden directory
    /// the audit could not finish, and an entry whose name no key can
    /// carry (a directory never entered, a link, a `.nml`-named entry —
    /// under the directory holding it), and a directory at the
    /// component bound (never listed) —
    /// errors; a symlink whose name is not `.nml`-shaped is a warning
    /// (what lies beneath it is exactly what the walk never learns).
    /// Policy directories (`node_modules`, `target`) are rows on the
    /// closing row only. A gate that stayed silent over these certified
    /// a tree it never looked at: the walked link failed only when named.
    pub fn unjudged_under(&self, dirs: &[SourceKey]) -> Vec<Diagnostic> {
        let closed = self.discovery.universe().is_closed();
        let mut rows = Vec::new();
        // One audit budget for the run: every hidden directory under the
        // named targets shares it.
        let mut budget = AuditBudget::default();
        for skipped in self.discovery.skipped() {
            if !dirs.iter().any(|d| d.contains(&skipped.key)) {
                continue;
            }
            let key = &skipped.key;
            match &skipped.why {
                // The rows are the kernel's sentences (`diag`, the one
                // code-minting site); this verb decides only WHICH
                // skipped entries a gate certifies and audits the
                // hidden ones — ONE row per hidden directory.
                Skip::DotDirectory => {
                    let audit = audit_hidden(self.discovery.root(), &StdFs, key, &mut budget);
                    rows.extend(skipped_under(key, &audit));
                    rows.extend(audit.incomplete.as_ref().map(audit_incomplete));
                }
                Skip::Symlink
                | Skip::Fifo
                | Skip::DotFile
                | Skip::PolicyDirectory
                | Skip::UnkeyableName { .. }
                | Skip::ComponentBound => {
                    rows.extend(skipped_row(skipped, closed));
                }
            }
        }
        // A budget unit the walk STOPPED INSIDE is the same fact as a
        // skipped entry — content under the named directories that no
        // verb judged — and it is the one the walk itself never lists:
        // the unit's files are purged, so no target carries its row.
        // Without it a directory run certified the denied subtree with
        // exit 0, and only the closing `--json` row's `truncatedUnits`
        // said otherwise.
        rows.extend(self.discovery.unit_errors_under(dirs));
        rows
    }

    /// The root as every row states it (`summary`, `binding`).
    pub fn root_facts(&self) -> crate::out::RootFacts {
        root_facts_of(self.discovery.root())
    }

    /// The root-origin tag the CLI prints after the root path
    /// ([`origin_tag_of`]).
    pub fn origin_tag(&self) -> String {
        origin_tag_of(self.discovery.root())
    }

    /// The universe this invocation resolved in — the kernel's
    /// [`Discovery`], read directly for its root, its universe, the
    /// directive vocabulary covering a schema source, the inert notes on a
    /// key and a live manifest's text. The `Workspace` adds only what the
    /// CLI owns: the `--root` advice on a truncation row
    /// ([`Self::universe_notes`]), `line:col` rendering
    /// ([`Self::locate`], [`Self::manifest_location`]) and the target
    /// discipline.
    pub fn discovery(&self) -> &Discovery {
        &self.discovery
    }

    /// A target outside the root ([`outside_root_of`]): marked here for
    /// the verbs that resolve without expanding (`binding`), and the
    /// structural backstop of [`Self::expand_targets`].
    fn outside_root(&self, path: &Path) -> String {
        outside_root_of(self.discovery.root(), path)
    }

    /// The target's key, governing binding and kernel findings — computed
    /// BEFORE the file is read (a rejected path is never opened; E26).
    /// Kernel failures that are not diagnostics render as CLI errors
    /// naming the authored path only.
    pub fn resolve(&self, path: &Path) -> Result<Resolved<'_>, String> {
        let abs = absolute(path)?;
        let mut resolved =
            resolve_file(&self.discovery.universe(), &abs, &StdFs).map_err(|e| match e {
                PathError::Escapes { .. } => self.outside_root(path),
                other => format!("{}: {other}", path.display()),
            })?;
        // A rejection of a path spelled through `..`: the key is the
        // lexically-popped spelling, so the component the finding names
        // (`lib` in `tenants/cu/lib/../plain.flow.nml`) need not appear
        // in it — the kernel re-renders its own finding naming the
        // spelling AS TYPED beside the key. Only a `..` spelling differs;
        // every other rejection keeps the key alone.
        let spelled_through_dotdot = path
            .components()
            .any(|c| c == std::path::Component::ParentDir);
        let typed = path.display().to_string();
        if let Some(finding) = resolved
            .rejection
            .as_ref()
            .filter(|_| spelled_through_dotdot)
            .and_then(|err| path_finding_typed(err, Some(&typed)))
        {
            resolved.findings = vec![finding];
        }
        Ok(resolved)
    }

    /// Read the checked file THROUGH the verdict (E35, sec 1): the text
    /// of [`Self::open_target`], for the verbs that never write.
    pub fn read_target(&self, resolved: &Resolved<'_>, path: &Path) -> Result<String, String> {
        self.open_target(resolved, path).map(|opened| opened.text)
    }

    /// Read the checked file THROUGH the verdict (E35, sec 1): the
    /// verified leaf must be a regular file — a FIFO, a device, a socket
    /// named as the target is refused BEFORE any open, never blocked on
    /// (sec 1c) — and in a closed universe the bytes come from
    /// `open_beneath` at the KEY the kernel minted: `openat` per
    /// component with `O_NOFOLLOW`, anchored at the root, so a directory
    /// swapped for a symlink after the kernel classified it is refused
    /// at the open, never followed ("the key you minted is the bytes you
    /// read"). An open universe follows the operator's own links by
    /// design — ONCE, at the leaf: the
    /// typed leaf is resolved to the file it names, the bytes come from
    /// `open_beneath` at that file's own parent with `O_NOFOLLOW`, and
    /// the same [`TargetLeaf`] rides back with the text for
    /// [`Self::write_target`], so a link re-pointed between the read and
    /// the write changes nothing (a read by path with the write resolving
    /// the link a second time would). The following `stat`
    /// still refuses a non-regular endpoint before any open.
    pub fn open_target(&self, resolved: &Resolved<'_>, path: &Path) -> Result<Opened, String> {
        let refuse = |name: &str| {
            Err(format!(
                "failed to read {}: `{name}` is not a regular file — a FIFO, socket or \
                 device is never opened; replace it with a regular file",
                path.display()
            ))
        };
        if resolved.kind == Some(EntryKind::Other) {
            return refuse(resolved.key.file_name());
        }
        let (text, leaf) = if !self.discovery.universe().is_closed() {
            if let Ok(meta) = std::fs::metadata(path) {
                let kind = meta.file_type();
                // A typed LINK to a directory resolves to one: the walk's
                // sentence, as for a directory it never entered — never
                // the OS's `Is a directory` from the open below.
                if kind.is_dir() {
                    return Err(directory_not_entered(path));
                }
                if !kind.is_file() {
                    return refuse(resolved.key.file_name());
                }
            }
            let at = crate::leaf_under_parent(path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            // The kernel's one reader at the resolved leaf, under the
            // target's own cap (A4): the same take-cap-plus-one
            // discipline as every discovery read, so a 100 MB tenant
            // file is refused at the bound, never held.
            let text = read_beneath(
                &at.dir,
                &[at.name.as_str()],
                MAX_TARGET_BYTES,
                "a check target",
            )
            .map_err(|e| {
                let why = match e {
                    // The leaf became a link: the CLI's own advice.
                    ReadError::Open(e @ nml_validate::fs::OpenError::Symlink { .. }) => {
                        crate::leaf_advice(e)
                    }
                    // Everything else in the OS's own words, as the
                    // by-path read spelled it (the reader's `Display`).
                    e => e.to_string(),
                };
                format!("failed to read {}: {why}", path.display())
            })?;
            (text, TargetLeaf::Resolved(at))
        } else {
            let components: Vec<&str> = resolved.key.components().collect();
            let text = read_beneath(
                self.discovery.root().path(),
                &components,
                MAX_TARGET_BYTES,
                "a check target",
            )
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            (text, TargetLeaf::Key)
        };
        Ok(Opened { text, leaf })
    }

    /// Read a `Related.source` the way the target is read: the name is a
    /// KEY (step 0f), turned back into a path through the root's one
    /// door (`root.path_of`), resolved under THIS universe and read
    /// through [`Self::read_target`] — a note's file is never read by
    /// cwd-relative path outside the kernel's walk. A name that is no key
    /// (a `--schema` source's basename, a spelling outside the root) or
    /// one the kernel rejects (through a link under a closed binding) is
    /// unreadable here, and the note renders without a range.
    pub fn read_source(&self, name: &str) -> Result<String, String> {
        let key = SourceKey::checked(name).ok_or_else(|| format!("{name}: not a workspace key"))?;
        let path = self.discovery.root().path_of(&key);
        let path = path.as_path();
        let resolved = self.resolve(path)?;
        if resolved
            .findings
            .iter()
            .any(|d| d.severity == nml_core::diagnostic::Severity::Error)
        {
            return Err(format!("{name}: rejected by the universe"));
        }
        self.read_target(&resolved, path)
    }

    /// The fixer's write, through the same verdict (E35, sec 1b): in a
    /// closed universe the file at the KEY is replaced through the
    /// chain's parent descriptor (`write_beneath`: `openat(parent_fd,
    /// O_CREAT | O_EXCL | O_NOFOLLOW)` + `renameat`), so a closed-
    /// universe fix never writes by path; an open universe writes at
    /// the leaf [`Self::open_target`] resolved — the same file the read
    /// came from. Non-unix keeps the path writer everywhere
    /// (documented: E23's boundary stands on that lane).
    pub fn write_target(
        &self,
        resolved: &Resolved<'_>,
        leaf: &TargetLeaf,
        path: &Path,
        contents: &str,
    ) -> Result<(), String> {
        match leaf {
            TargetLeaf::Key => {
                let components: Vec<&str> = resolved.key.components().collect();
                write_under(self.discovery.root(), &components, path, contents)
            }
            TargetLeaf::Resolved(at) => crate::write_file_atomically(path, at, contents),
        }
    }

    /// The universe's word on every file under it, stated ONCE per run
    /// by every verb: its ERRORS — a truncated walk (NML2089, naming
    /// where it stopped), live manifests or configs that failed to load
    /// (NML2088) — or, when it stands, its unit-layout notes (NML2092);
    /// the kernel's rows and the kernel's rule (`Discovery::
    /// universe_notes`), with the one sentence only this front end can
    /// add: the truncation's advice, pass `--root` to a smaller tree (the
    /// kernel states the fact, the CLI owns the flag — E35). A universe
    /// with an error validates nothing — every checking verb counts
    /// these and exits 1 (E28), never degrading to parse-only checking.
    pub fn universe_notes(&self) -> Vec<Diagnostic> {
        self.with_root_advice(self.discovery.universe_notes())
    }

    /// Where a universe note spanned in a MANIFEST sits (`line`, `column`,
    /// 1-based): the kernel's one derivation over the manifest's own text
    /// (`Discovery::manifest_location` — a loaded package's, or an
    /// unloadable manifest's kept for this; no second read), so the CLI
    /// prints it `key:line:col:` as it prints every located finding, and
    /// the `--json` row carries the location.
    pub fn manifest_location(&self, diag: &Diagnostic) -> Option<(usize, usize)> {
        self.discovery
            .manifest_location(diag)
            .map(|at| (at.line, at.column))
    }

    /// Where `span` sits in the live manifest keyed `source` (`line`,
    /// `column`, 1-based): the kernel's one derivation
    /// (`Discovery::locate`), the same that locates a universe row — so
    /// `key:line:col: note:` beneath a row, the `--json` `related[]`
    /// entry and a wrapping row's `cause` are derived once, from the
    /// text the verdict read. `None` for a content file or a span past
    /// the text.
    pub fn locate(&self, source: &str, span: nml_core::span::Span) -> Option<(usize, usize)> {
        self.discovery
            .locate(source, span)
            .map(|at| (at.line, at.column))
    }

    /// The CLI's sentence on the kernel's truncation row: `--root` is
    /// this front end's flag, so the advice is appended here and nowhere
    /// in Layer B (E35; the editor has no flag to offer). A spent
    /// live-input budget names an INPUT, not a directory, and its remedy
    /// is smaller live inputs — the kernel's own sentence ends in it —
    /// so `--root` would be the wrong advice and rides only the
    /// entry-bound, unreadable-directory and universe-wide byte-backstop
    /// rows (the backstop is the sixteen-tenant doctrine, where a smaller
    /// tree IS the remedy).
    /// Every other row passes through unchanged.
    fn with_root_advice(&self, mut notes: Vec<Diagnostic>) -> Vec<Diagnostic> {
        if matches!(
            self.discovery.truncated(),
            Some(Truncation::LiveInputBytes { .. })
        ) {
            return notes;
        }
        // "A smaller tree" alone would send the operator to root INSIDE
        // the flood's tenant, which leaves the manifest outside the
        // universe (the guide's own warning); the advice names both
        // remedies in the order they are usually right.
        for note in &mut notes {
            if note.code == Some(codes::UNIVERSE_TRUNCATED) {
                note.message
                    .push_str(", or pass --root to a smaller tree that still holds your manifests");
            }
        }
        notes
    }

    /// The workspace verbs' arguments EXPANDED to files — one expansion
    /// for `check`, `validate` and `fix`, so no two
    /// verbs can disagree about what a directory names. A file passes
    /// through as typed (whatever its extension — the operator named it
    /// deliberately); a directory expands to the `.nml` files the
    /// KERNEL'S ONE WALK saw under it (`Discovery::nml_files_under`):
    /// regular files only — a symlink, a FIFO, a policy-skipped subtree
    /// (`node_modules`, `target`, a dot-directory) and a dot-file are
    /// never among them — and no second walk under a second bound ever
    /// runs. Each expanded file is spelled through the argument AS TYPED
    /// (`vendor/../vendor/sub/x.nml` for `vendor/../vendor/sub`) and
    /// taken ONCE however many arguments reach it: the kernel's key is
    /// the identity, not the spelling. Argument order is kept — the
    /// first error names the run's explain hint, and a `--json`
    /// consumer reads the rows in the order it asked — with each
    /// directory's expansion sorted within it.
    ///
    /// An argument is classified by the KERNEL (`SourceKey::classify`,
    /// ONCE, in [`Self::open`] before the walk): the root, or a real directory the
    /// kernel's own walk reaches — entered through the operator's
    /// prefix above the root, `lstat`-first below it, halting at the
    /// first symlink — is expanded AT ITS KEY (`proj/nope/../vendor` is
    /// `Dir(vendor)` by E28's lexical pop, and enumerates `vendor`);
    /// everything else (a link, a halt, an absent or outside path, an
    /// empty spelling) is a file candidate exactly as authored, so the
    /// universe walk judges it (under a closed binding NML2083,
    /// byte-identical whether or not the link's target exists) and
    /// nothing about a target's existence is disclosed before
    /// resolution; a genuinely absent path fails at the read. A
    /// directory under a BUDGET UNIT the walk stopped inside is a file
    /// candidate too: the kernel already refused everything under it,
    /// and its resolution carries the unit's NML2089 — never an OS error
    /// from a walk that should not have started (an unlistable
    /// subdirectory is the kernel's NML2089, universe-wide or per unit,
    /// by construction). Arguments that name no `.nml` file at all fail
    /// the run, naming them.
    pub fn expand_targets(&self) -> Result<Expanded, String> {
        let mut files: Vec<String> = Vec::new();
        let mut dirs: Vec<SourceKey> = Vec::new();
        let mut taken: HashSet<SourceKey> = HashSet::new();
        let mut named: HashSet<&str> = HashSet::new();
        for (arg, verdict) in &self.classified {
            let p = Path::new(arg);
            // The kernel classified every argument ONCE, at `open`,
            // before the walk: the directories it names are what the
            // gate certifies too (`dirs`). An empty spelling never
            // arrives: the parser refuses it as a usage error, and the
            // kernel's `mint` would refuse it as no path.
            let dir = match verdict {
                Ok(Endpoint::Root) => {
                    dirs.push(SourceKey::root());
                    Some(SourceKey::root())
                }
                Ok(Endpoint::Dir(key)) => {
                    dirs.push(key.clone());
                    self.discovery
                        .universe()
                        .truncated_unit(key)
                        .is_none()
                        .then(|| key.clone())
                }
                // Outside the root: refused at `open`, before the walk —
                // this arm is the structural backstop, never reached.
                Err(PathError::Escapes { .. }) => return Err(self.outside_root(p)),
                Ok(Endpoint::Other) | Err(_) => None,
            };
            let Some(dir) = dir else {
                if named.insert(arg.as_str()) {
                    files.push(arg.clone());
                }
                continue;
            };
            let mut expanded: Vec<String> = self
                .discovery
                .nml_files_under(&dir)
                .filter(|key| taken.insert((*key).clone()))
                .map(|key| {
                    let under = key.relative_to(&dir).unwrap_or(key.as_str());
                    p.join(under).display().to_string()
                })
                .collect();
            expanded.sort();
            files.extend(expanded);
        }
        if files.is_empty() {
            let named: Vec<String> = self
                .classified
                .iter()
                .map(|(p, _)| format!("`{}`", crate::sanitized(p)))
                .collect();
            return Err(format!("no .nml files found under {}", named.join(", ")));
        }
        let set = self.classified.len() > 1 || !dirs.is_empty();
        Ok(Expanded { files, dirs, set })
    }
}

/// The root as every row states it (`summary`, `binding`).
pub(crate) fn root_facts_of(root: &WorkspaceRoot) -> crate::out::RootFacts {
    let (fence, shadowed) = match root.origin() {
        RootOrigin::Derived { fence, shadowed } => (
            fence.entry_tag(),
            shadowed.as_ref().map(|s| wire_root_path(s.path())),
        ),
        RootOrigin::Explicit | RootOrigin::Editor => (None, None),
    };
    crate::out::RootFacts {
        path: wire_root_path(root.path()),
        origin: root.origin().tag(),
        fence,
        shadowed,
    }
}

/// The root-origin tag the CLI prints after the root path: the
/// kernel's FACT (`RootOrigin::label`, stable for scripts) rendered
/// as this front end's sentence, with the advice only the CLI can
/// give — `--root` is its flag (E35: Layer A carries no CLI text).
pub(crate) fn origin_tag_of(root: &WorkspaceRoot) -> String {
    match root.origin() {
        RootOrigin::Explicit => "(--root)".to_string(),
        RootOrigin::Editor => "(editor)".to_string(),
        // The fence entry's kind and the shadow are the facts an
        // operator reading a derived verdict must see (a `.git` FILE
        // is a worktree's, a submodule's or a planted one) — the
        // kernel's ONE sentence (`RootOrigin::fence_facts`, the
        // editor's log line says the same), its paths spelled as this
        // front end spells the root beside it (`display_path`), then
        // this front end's advice tail: the flag that pins, and — for
        // a marker — the concrete `--root` that checks under the
        // marker's universe (its directory, spelled, not "at its
        // directory"). The tag states what the kernel knows — the root
        // was derived within this fence — never "outermost manifest",
        // which an OPEN context (no manifest within the fence at all)
        // contradicted on the same screen.
        RootOrigin::Derived { shadowed, .. } => match root.origin().fence_facts(&display_path) {
            Some(facts) => {
                let alternative = match shadowed {
                    Some(Shadow::Marker(marker)) => format!(
                        ", or --root {} to check under that universe",
                        crate::sanitized(&marker.parent().map(display_path).unwrap_or_default())
                    ),
                    Some(Shadow::Git(_)) | None => String::new(),
                };
                format!(
                    "(derived {} — pass --root to pin{alternative})",
                    crate::sanitized(&facts)
                )
            }
            // "the workspace root", not "the universe": the label on
            // the same line already says `workspace root`, and the word
            // `universe` carries two other senses here — the open/closed
            // world (`universe` on the `--json` row) and `--schema`'s
            // set of schema sources. One line, one sense.
            None => "(derived: no .git fence found, so the target's own directory is the \
                     workspace root — pass --root to pin)"
                .to_string(),
        },
    }
}

/// A directory that reached a per-file pipeline — one the universe walk
/// did NOT enter (behind a link an open universe follows only when
/// named, or inside a denied unit), or a typed link whose resolved leaf
/// is one — refused in the walk's own words by every verb, never read
/// as a file (the OS's `Is a directory`). The ONE sentence
/// `refuse_directory` and `open_target` share.
pub(crate) fn directory_not_entered(path: &Path) -> String {
    format!(
        "`{}` is a directory the universe walk did not enter (behind a link, or inside a \
         denied unit) — name the files, or pass --root to a tree the walk can list",
        path.display()
    )
}

/// A target outside the root is the INVOCATION's mistake — the flag
/// (or the derived root) names a universe the file is not in — so it
/// is a usage error, exit 2, in every verb: refused before any target
/// runs by [`Workspace::expand_targets`], and marked here for the verbs
/// that resolve without expanding (`binding`). It used to be a
/// per-target failure `check`/`validate`/`fix` continued past (exit
/// 1) while `binding` exited 2 for the same sentence.
fn outside_root_of(root: &WorkspaceRoot, path: &Path) -> String {
    // Under a DERIVED root the tag's `pass --root to pin` cannot make
    // two universes one: a file in another universe is checked in
    // its own run.
    let own_run = match root.origin() {
        RootOrigin::Explicit => "",
        _ => "; a file in another universe is checked in its own run",
    };
    crate::out::usage_error(format!(
        "{} is outside the workspace root `{}` {}{own_run}",
        path.display(),
        display_path(root.path()),
        origin_tag_of(root)
    ))
}

/// The workspace verbs' arguments after [`Workspace::expand_targets`]:
/// the files to run, spelled through the arguments as typed, and the
/// directory keys the arguments named — the directories a gate
/// certifies ([`Workspace::unjudged_under`]), classified once with the
/// expansion.
pub struct Expanded {
    pub files: Vec<String>,
    pub dirs: Vec<SourceKey>,
    /// The operator asked about a SET — a directory was named, or more
    /// than one path — so the run ends with one closing tally on
    /// success; a single path's own `ok` line is its whole verdict.
    /// Keyed on the invocation's shape, never on how many files it
    /// expanded to, so a directory run ends the same way however many
    /// files it held.
    pub set: bool,
}

/// Where a target's bytes came from, carried from the read to the write.
pub enum TargetLeaf {
    /// A closed universe: the write re-walks the kernel's KEY from the
    /// root through the chain's parent descriptor — nothing to carry.
    Key,
    /// An open universe: the operator's typed leaf resolved ONCE at the
    /// read — its parent and its own name, a link followed once — so
    /// the write lands on the file the read came from whatever the
    /// link points at by then.
    Resolved(crate::LeafAt),
}

/// A target's text and where it came from ([`Workspace::open_target`]).
pub struct Opened {
    pub text: String,
    pub leaf: TargetLeaf,
}

/// The `(unit, stop, why)` rows of the budget units the walk stopped
/// inside, as both the `summary` and the `binding` row carry them
/// (`SourceKey` via `Display` — the spelling the NML2089 sentence uses;
/// `why` the kernel's `UnitBound::tag`).
pub fn truncated_units(universe: &Universe<'_>) -> Vec<(String, String, &'static str)> {
    universe
        .truncated_units
        .iter()
        .map(|t| (t.unit.to_string(), t.stop.to_string(), t.why.tag()))
        .collect()
}

#[cfg(unix)]
fn write_under(
    root: &WorkspaceRoot,
    components: &[&str],
    path: &Path,
    contents: &str,
) -> Result<(), String> {
    nml_validate::fs::write_beneath(root.path(), components, contents.as_bytes())
        .map_err(|e| format!("failed to write {}: {}", path.display(), e.into_io()))
}

#[cfg(not(unix))]
fn write_under(
    _root: &WorkspaceRoot,
    _components: &[&str],
    path: &Path,
    contents: &str,
) -> Result<(), String> {
    crate::write_file_atomically(path, &crate::leaf_under_parent(path)?, contents)
}

/// D-0d-1: a claim-governed file validates under its binding's package
/// (one matcher governs validation AND composition); `--schema` beside a
/// governing WORKSPACE-manifest binding is a hard error (a CI flag
/// cannot substitute another vocabulary inside a closed universe); open
/// universes, unbound files, and files bound only by the builtin or a
/// store package keep the flag's behavior verbatim. `Ok(None)` = the
/// flags decide (today's path). An AMBIGUOUSLY-claimed file is never
/// `Ok(None)`: it validates under no binding, and "the flags decide"
/// would be parse-only checking with a green exit.
pub fn judge(
    resolved: &Resolved<'_>,
    schema_dir: Option<&Path>,
    file: &Path,
) -> Result<Option<Arc<SchemaValidator>>, Conflict> {
    let claimant = match &resolved.governing {
        Governing::Bound { claimant, .. } => claimant,
        Governing::Unbound => return Ok(None),
        // Rule 3, closed-denied. In practice the kernel's ambiguity
        // FINDING (error) fails the verb through the standard tally
        // before anything is judged — exit 1, the file never read — so
        // this arm is the backstop. It is also the one-line switch
        // should the owner prefer the `--schema`-conflict shape (exit
        // 2): drop the finding in `resolve_file` and this conflict
        // becomes the verb's answer.
        Governing::Ambiguous(claimants) => {
            return Err(Conflict(format!(
                "{}\n  --> {}\n  \
                 = an ambiguously-claimed file validates under no binding; remove or narrow \
                 one claim\n  \
                 = run `nml binding {}` to see every claimant",
                ambiguous_claim_summary(claimants),
                file.display(),
                file.display()
            )));
        }
    };
    // The conflict is with a MANIFEST the operator committed: the
    // builtin (which claims every `*.package.nml`) and the store are not
    // discovered manifests, and `--schema` beside them keeps today's
    // meaning ("validate this file against these schemas").
    let governed_by_manifest = matches!(claimant.claim.origin(), ClaimOrigin::Workspace { .. });
    match schema_dir {
        Some(dir) if governed_by_manifest => Err(Conflict(format!(
            "--schema {} conflicts with the manifest that governs this file\n  \
             --> {}\n  \
             = binding '{}' of {} claims it (files[{}] = {:?})\n  \
             = a manifest-governed file validates under its binding's package; a CI flag \
             cannot substitute another vocabulary inside a closed universe\n  \
             = fix: drop --schema (the binding supplies the schema), or narrow the \
             manifest's files glob if this file should not be claimed\n  \
             = run `nml binding {}` to see the governing claim",
            dir.display(),
            file.display(),
            claimant.binding.name,
            claimant.claim.manifest_label(),
            claimant.glob,
            claimant.binding.files[claimant.glob],
            file.display()
        ))),
        Some(_) => Ok(None),
        // The kernel built the binding's validator once for the universe
        // (`Resolved::validator`), under the binding's OWN strictness —
        // no flag's: a bound file's verdict is the binding's in every
        // front end (`--strict` is said not to apply, by the caller). A
        // binding that cannot build one is the kernel's NML2091 finding
        // on the file — an error every caller tallies BEFORE judging, so
        // this arm is the backstop, never the verdict: a bound file must
        // not fall through to "the flags decide", and reaching it means
        // a caller judged before it reported.
        None => resolved
            .validator
            .as_ref()
            .map(|v| Some(Arc::clone(v)))
            .ok_or_else(|| {
                Conflict(format!(
                    "binding '{}' of {} cannot build its validator, and its NML2091 finding \
                     was not reported before judging (a front-end invariant broke) — the file \
                     validates under no binding",
                    claimant.binding.name,
                    claimant.claim.manifest_label()
                ))
            }),
    }
}

/// A usage/configuration error (exit code 2): the invocation contradicts
/// the universe it runs in.
pub struct Conflict(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;
    use nml_core::diagnostic::Severity;
    use nml_validate::workspace::input_cap;

    /// The `workspace` fixture, opened as the CLI opens it.
    fn fixture_workspace() -> (PathBuf, Workspace) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace");
        let target = root.join("shared/x.flow.nml");
        let ws = Workspace::open(Some(&root), &[target.display().to_string()])
            .expect("the fixture opens");
        (target, ws)
    }

    /// An ambiguously-claimed file never resolves to "the flags decide".
    /// The kernel carries the denial as an error finding naming both
    /// manifests (the tally every verb counts), and the judge itself
    /// refuses rather than answering `Ok(None)` — silent, `check` would
    /// validate the file parse-only.
    #[test]
    fn ambiguous_claim_is_never_judged_parse_only() {
        let (target, ws) = fixture_workspace();
        let resolved = ws.resolve(&target).expect("resolves");
        assert!(matches!(resolved.governing, Governing::Ambiguous(_)));
        let [finding] = resolved.findings.as_slice() else {
            panic!("one finding, got {:?}", resolved.findings);
        };
        assert_eq!(finding.severity, Severity::Error);
        assert_eq!(finding.source.as_deref(), Some("shared/x.flow.nml"));
        assert!(
            finding.message.starts_with(
                "2 manifests claim this file: demo.package.nml (shared, files[0] = \
                 \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \
                 \"shared/**/*.flow.nml\") — an ambiguously-claimed file is denied"
            ),
            "{}",
            finding.message
        );
        for schema_dir in [None, Some(Path::new("schemas"))] {
            match judge(&resolved, schema_dir, &target) {
                Err(Conflict(message)) => {
                    assert!(
                        message.contains("demo.package.nml")
                            && message.contains("other.package.nml"),
                        "{message}"
                    );
                }
                Ok(v) => panic!(
                    "judged {}: an ambiguous claim must not resolve to a validator or to the flags",
                    if v.is_some() {
                        "a validator"
                    } else {
                        "Ok(None)"
                    }
                ),
            }
        }
    }

    /// The closed-universe fixer's write goes
    /// THROUGH `write_beneath` — the seam, not just the function. A
    /// closed workspace is opened and a file resolved; then its parent
    /// directory is swapped for a symlink to a directory outside the
    /// root. `write_target` must refuse at `cu` (the chain's `O_NOFOLLOW`
    /// parent open) and the outside directory must stay empty — the
    /// path writer would have written `outside/<leaf>` through the link.
    /// Timing-free: the swap happens between resolve and write.
    #[cfg(unix)]
    #[test]
    fn closed_universe_write_target_refuses_a_swapped_parent() {
        let dir = Scratch::new("write-target");
        let root = dir.join("proj");
        std::fs::create_dir_all(root.join("tenants/cu")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace");
        for name in ["demo.package.nml", "core.model.nml"] {
            std::fs::copy(fixture.join(name), root.join(name)).unwrap();
        }
        let file = root.join("tenants/cu/f.flow.nml");
        std::fs::write(&file, "old\n").unwrap();
        let ws = Workspace::open(Some(&root), &[file.display().to_string()])
            .expect("the workspace opens");
        assert!(
            ws.discovery().universe().is_closed(),
            "one live workspace manifest closes it"
        );
        let resolved = ws.resolve(&file).expect("resolves");
        assert_eq!(resolved.key.as_str(), "tenants/cu/f.flow.nml");

        std::fs::rename(root.join("tenants/cu"), root.join("tenants/cu.real")).unwrap();
        std::os::unix::fs::symlink("../../outside", root.join("tenants/cu")).unwrap();
        let err = ws
            .write_target(&resolved, &TargetLeaf::Key, &file, "x")
            .expect_err("a parent swapped for a link is refused");
        assert!(
            err.contains("path component `cu` is a symlink (refused at open)"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_dir(dir.join("outside")).unwrap().count(),
            0,
            "nothing was written through the link"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("tenants/cu.real/f.flow.nml")).unwrap(),
            "old\n",
            "the real file is untouched"
        );
    }

    /// The per-kind caps, through the reader this front end hands the
    /// walk (the kernel's `read_input`): a manifest or config reads under
    /// 256 KiB, a declared source under 4 MiB, and the refusal names the
    /// kind.
    #[test]
    fn discovery_reads_are_capped_per_kind() {
        assert_eq!(input_cap(InputKind::Manifest), 256 * 1024);
        assert_eq!(input_cap(InputKind::ProjectConfig), 256 * 1024);
        assert_eq!(input_cap(InputKind::Source), 4 * 1024 * 1024);
        let dir = Scratch::new("read-cap");
        let root = WorkspaceRoot::explicit(&dir, &StdFs).unwrap();
        let path = root.path().join("big.nml");
        std::fs::write(&path, vec![b' '; input_cap(InputKind::Manifest) + 1]).unwrap();
        assert_eq!(
            read_input(&root, InputKind::Manifest, &path).unwrap_err(),
            "too large: over 256 KiB (262145 bytes) — a package manifest is read only up to 256 KiB \
             (262144 bytes)"
        );
        assert_eq!(
            read_input(&root, InputKind::ProjectConfig, &path).unwrap_err(),
            "too large: over 256 KiB (262145 bytes) — a project config is read only up to 256 KiB \
             (262144 bytes)"
        );
        assert_eq!(
            read_input(&root, InputKind::Source, &path).unwrap().len(),
            input_cap(InputKind::Manifest) + 1,
            "the same bytes are a fine declared source"
        );
        // r80-cov (mutant L1 `>` → `>=` survived): the bound is INCLUSIVE.
        let exact = root.path().join("exact.nml");
        std::fs::write(&exact, vec![b' '; input_cap(InputKind::Manifest)]).unwrap();
        assert_eq!(
            read_input(&root, InputKind::Manifest, &exact)
                .unwrap()
                .len(),
            input_cap(InputKind::Manifest),
            "exactly the cap reads whole"
        );
        assert_eq!(
            read_input(&root, InputKind::ProjectConfig, &exact)
                .unwrap()
                .len(),
            input_cap(InputKind::ProjectConfig)
        );
    }

    /// r84-cov (mutant D29 survived): the gate holds ONE audit budget per
    /// run. The kernel pins the sharing (two hidden directories, the second
    /// cut short where the first left off); this pins the CLI's ownership
    /// — the budget is created once, above the loop over the walk's
    /// skipped rows, never per directory (a `.venv` and a `.next` under one
    /// target would otherwise each get the universe's whole backstop).
    #[test]
    fn the_gate_holds_one_audit_budget_per_run() {
        let src = include_str!("workspace.rs");
        let needle = concat!("AuditBudget::", "default()");
        let budgets: Vec<usize> = src.match_indices(needle).map(|(i, _)| i).collect();
        assert_eq!(budgets.len(), 1, "one budget per run: {}", budgets.len());
        let loop_at = src
            .find("for skipped in self.discovery.skipped() {")
            .expect("the gate's loop");
        assert!(
            budgets[0] < loop_at,
            "the budget is created before the loop, not per directory"
        );
    }

    /// The target cap: a 17 MiB sparse target is refused
    /// naming the 16 MiB bound — through the same take-cap-plus-one
    /// discipline, so the refusal is fast and holds no bytes past the
    /// cap — while a 1 MiB target reads whole. Both trusts: an open
    /// universe reads by path, a closed one through the chain.
    #[test]
    fn check_target_is_capped_at_sixteen_mib() {
        assert_eq!(MAX_TARGET_BYTES, 16 * 1024 * 1024);
        let dir = Scratch::new("target-cap");
        let big = dir.join("big.nml");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(17 * 1024 * 1024).unwrap();
        drop(file);
        let small = dir.join("small.nml");
        std::fs::write(&small, vec![b' '; 1024 * 1024]).unwrap();
        let ws = Workspace::open(Some(&*dir), &[big.display().to_string()]).expect("opens");
        assert!(!ws.discovery().universe().is_closed());
        let resolved = ws.resolve(&big).expect("resolves");
        let started = std::time::Instant::now();
        let err = ws.read_target(&resolved, &big).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(
            err.contains("a check target is read only up to 16 MiB (16777216 bytes)")
                && err.contains("17 MiB (17825792 bytes)"),
            "{err}"
        );
        let resolved = ws.resolve(&small).expect("resolves");
        assert_eq!(
            ws.read_target(&resolved, &small).unwrap().len(),
            1024 * 1024
        );
        // r80-cov (mutant L1 survived): exactly MAX_TARGET_BYTES reads
        // whole; one byte more is refused naming both sizes.
        let exact = dir.join("exact.nml");
        std::fs::File::create(&exact)
            .unwrap()
            .set_len(MAX_TARGET_BYTES as u64)
            .unwrap();
        let over = dir.join("over.nml");
        std::fs::File::create(&over)
            .unwrap()
            .set_len(MAX_TARGET_BYTES as u64 + 1)
            .unwrap();
        let resolved = ws.resolve(&exact).expect("resolves");
        assert_eq!(
            ws.read_target(&resolved, &exact).unwrap().len(),
            MAX_TARGET_BYTES,
            "the bound is inclusive"
        );
        let resolved = ws.resolve(&over).expect("resolves");
        let err = ws.read_target(&resolved, &over).unwrap_err();
        assert!(
            err.contains("over 16 MiB (16777217 bytes)")
                && err.contains("a check target is read only up to 16 MiB (16777216 bytes)"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod manifest_location_tests {
    use super::*;

    /// A universe note is located in a manifest's own text only when its
    /// source IS a live manifest's key and its span lies within that
    /// text: a span past the end (a stale offset), a source that is a
    /// content file, or no span at all locate nothing — the row prints
    /// locationless, and the reader never indexes past the text.
    #[test]
    fn manifest_location_is_none_for_a_foreign_source_or_a_span_past_the_text() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace-gap");
        let target = root.join("tenants/cu/flows/plain.flow.nml");
        let ws = Workspace::open(Some(&root), &[target.display().to_string()])
            .expect("the fixture opens");
        let claim = ws
            .discovery
            .claims()
            .iter()
            .find(|c| c.manifest().is_some())
            .expect("a live manifest");
        let manifest = claim.manifest().expect("keyed").to_string();
        let text_len = claim.package.manifest_text.len();
        let at = |source: &str, start: usize| {
            let diag = Diagnostic::warning("x")
                .with_source(source.to_string())
                .with_span(nml_core::span::Span::new(start, start + 1));
            ws.manifest_location(&diag)
        };
        assert_eq!(at(&manifest, 0), Some((1, 1)));
        assert_eq!(at(&manifest, text_len + 1), None, "past the text");
        assert_eq!(
            at("tenants/cu/flows/plain.flow.nml", 0),
            None,
            "a content file"
        );
        let spanless = Diagnostic::warning("x").with_source(manifest.clone());
        assert_eq!(ws.manifest_location(&spanless), None, "no span");
        let sourceless = Diagnostic::warning("x").with_span(nml_core::span::Span::new(0, 1));
        assert_eq!(ws.manifest_location(&sourceless), None, "no source");
    }

    /// A PLACE in a live manifest — a note's, a cause's — locates through
    /// the same derivation as the row: over the text the universe kept, a
    /// manifest that FAILED to load included (the first `files` beneath
    /// NML2088's row) — and never in a content file or past the text.
    #[test]
    fn locate_finds_a_place_in_a_kept_manifest_and_nothing_else() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace-dup");
        let target = root.join("tenants/cu/plain.flow.nml");
        let ws = Workspace::open(Some(&root), &[target.display().to_string()])
            .expect("the fixture opens");
        let text = ws
            .discovery()
            .manifest_text("demo.package.nml")
            .expect("a failed manifest's text is kept");
        let first_files = text.find("files:").expect("the block spelling");
        let row = Diagnostic::error("x")
            .with_source("demo.package.nml".to_string())
            .with_span(nml_core::span::Span::new(0, 1))
            .with_related(
                nml_core::span::Span::new(first_files, first_files + 5),
                "first",
            )
            .with_related_in(
                nml_core::span::Span::new(0, 1),
                "content",
                Some("tenants/cu/plain.flow.nml".to_string()),
            )
            .with_related(
                nml_core::span::Span::new(text.len() + 1, text.len() + 2),
                "stale",
            );
        let place = |i: usize| {
            let rel = &row.related[i];
            ws.locate(row.related_source(rel).expect("a file"), rel.span)
        };
        assert_eq!(place(0), Some((11, 9)));
        assert_eq!(place(1), None, "a content file");
        assert_eq!(place(2), None, "past the text");
        assert!(
            ws.discovery()
                .manifest_text("tenants/cu/plain.flow.nml")
                .is_none()
        );
    }
}

// unix only: the one test here exercises `open_beneath`'s `openat`/`O_NOFOLLOW`
// chain, so on other targets the module (and its imports) has nothing to compile.
#[cfg(all(test, unix))]
mod read_through_tests {
    use super::*;
    use crate::scratch::Scratch;

    /// E35 sec 1: in a closed universe the target's bytes come from
    /// `open_beneath` at the KEY the kernel minted — `openat` per
    /// component with `O_NOFOLLOW` — so a parent swapped for a symlink
    /// AFTER classification is refused at the open, never followed: the
    /// read never returns another file's bytes. (The write half was
    /// pinned — `closed_universe_write_target_refuses_a_swapped_parent`;
    /// a by-path `File::open` survived every read pin.)
    #[cfg(unix)]
    #[test]
    fn closed_universe_read_target_refuses_a_swapped_parent() {
        let dir = Scratch::new("read-target");
        let root = dir.join("proj");
        std::fs::create_dir_all(root.join("tenants/cu")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace");
        for name in ["demo.package.nml", "core.model.nml"] {
            std::fs::copy(fixture.join(name), root.join(name)).unwrap();
        }
        let file = root.join("tenants/cu/f.flow.nml");
        std::fs::write(&file, "mine\n").unwrap();
        std::fs::write(dir.join("outside/f.flow.nml"), "theirs\n").unwrap();
        let ws = Workspace::open(Some(&root), &[file.display().to_string()])
            .expect("the workspace opens");
        assert!(ws.discovery().universe().is_closed());
        let resolved = ws.resolve(&file).expect("resolves");
        assert_eq!(resolved.key.as_str(), "tenants/cu/f.flow.nml");
        assert_eq!(ws.read_target(&resolved, &file).unwrap(), "mine\n");

        std::fs::rename(root.join("tenants/cu"), root.join("tenants/cu.real")).unwrap();
        std::os::unix::fs::symlink("../../outside", root.join("tenants/cu")).unwrap();
        let err = ws
            .read_target(&resolved, &file)
            .expect_err("a parent swapped for a link is refused at the open");
        assert!(
            err.contains("path component `cu` is a symlink (refused at open)"),
            "{err}"
        );
        assert!(
            !err.contains("theirs"),
            "never the other file's bytes: {err}"
        );
    }
}
