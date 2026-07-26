//! `nml fix` (RFC 0017 §4.1) — the batch applier of machine-applicable
//! suggestions, the missing half of the stability policy's "breaking
//! changes ship with fixers" commitment. `NML0001` promised mechanical
//! migration since RFC 0006; until this command, that promise terminated
//! in an editor quick-fix.
//!
//! The rules, in order of what they protect:
//!
//! * **Sole-candidacy.** A suggestion is applied only when it is the sole
//!   candidate for its span: its diagnostic carries exactly one
//!   suggestion, and no other diagnostic proposes a *different* edit for
//!   the same span. This keys on candidacy, not on
//!   [`SuggestionKind`](nml_core::diagnostic::SuggestionKind) — kind
//!   describes exclusivity (RFC 0015's axis), not applicability — and it
//!   upholds RFC 0015's rule by construction: N mutually exclusive fixes
//!   are N candidates, so they never auto-apply.
//! * **Highest-offset-first splicing** ([`nml_core::cst::edit::splice`]),
//!   so earlier edits cannot invalidate later spans.
//! * **Re-check and revert.** Every round's result is re-analyzed before
//!   it is accepted; a round that does not strictly improve the file is
//!   discarded. The check runs on the in-memory candidate *before* any
//!   write — strictly safer than write-then-revert, with the same
//!   guarantee: a fixer that can worsen a file is worse than none. Writes
//!   go through the same atomic writer `fmt` uses.
//! * **Rounds to a fixpoint** (bounded): parse-layer fixes (`=>` → `->`)
//!   can unblock validation-layer fixes (`"30s"` → `30s`), which only
//!   become visible once the file parses; each round re-derives
//!   diagnostics from the current text.

use std::path::{Path, PathBuf};

use nml_core::cst::edit::{SpliceEdit, splice};
use nml_core::diagnostic::Diagnostic;
use nml_validate::schema::SchemaValidator;

/// Bound on fix rounds per file. Two layers (parse, then validation) plus
/// headroom for fixes that reveal fixes; a file needing more is beyond
/// mechanical repair and keeps its remaining diagnostics reported.
const MAX_ROUNDS: usize = 8;

pub fn cmd_fix(args: &[String]) -> Result<(), String> {
    let mut schema_dir: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut path_args: Vec<&String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--schema" => {
                i += 1;
                schema_dir =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        "--schema requires a path argument".to_string()
                    })?));
            }
            "--dry-run" => dry_run = true,
            flag if flag.starts_with("--") => {
                return Err(format!(
                    "unknown flag {flag}; usage: nml fix [--schema <dir>] [--dry-run] <path>..."
                ));
            }
            _ => path_args.push(&args[i]),
        }
        i += 1;
    }
    if path_args.is_empty() {
        return Err("usage: nml fix [--schema <dir>] [--dry-run] <path>...".to_string());
    }

    let files = collect_nml_files(&path_args)?;
    if files.is_empty() {
        return Err("no .nml files found under the given paths".to_string());
    }

    let mut fixed_files = 0usize;
    let mut total_edits = 0usize;
    let mut remaining = 0usize;
    for path in &files {
        let outcome = fix_file(path, schema_dir.as_ref(), dry_run)?;
        if outcome.applied > 0 {
            fixed_files += 1;
            total_edits += outcome.applied;
            let verb = if dry_run { "would fix" } else { "fixed" };
            println!("{verb} {} ({} edit(s))", path.display(), outcome.applied);
        }
        remaining += outcome.remaining;
    }
    let noun = if dry_run { "appliable" } else { "applied" };
    println!(
        "{total_edits} edit(s) {noun} across {fixed_files} of {} file(s); {remaining} diagnostic(s) not auto-fixable",
        files.len()
    );
    Ok(())
}

/// Expand path arguments to `.nml` files: files pass through (whatever
/// their extension — the user named them deliberately), directories are
/// walked recursively for `*.nml`, skipping dot-directories and symlinks
/// (a link cycle would otherwise recurse forever, and a link pointing
/// outside the named tree would silently widen what the user asked to
/// rewrite — follow-nothing is the safe default for a tool that writes).
/// Sorted and deduplicated for deterministic output.
fn collect_nml_files(paths: &[&String]) -> Result<Vec<PathBuf>, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let p = entry.path();
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            // `file_type()` reads the entry itself (lstat semantics), so a
            // symlinked directory never recurses and a symlinked file is
            // never rewritten through the link.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if hidden || file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&p, out)?;
            } else if p.extension().and_then(|e| e.to_str()) == Some("nml") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    for arg in paths {
        let p = PathBuf::from(arg);
        if p.is_dir() {
            walk(&p, &mut files)?;
        } else if p.is_file() {
            files.push(p);
        } else {
            return Err(format!("no such file or directory: {arg}"));
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

struct FixOutcome {
    /// Edits applied (or, dry-run, that would be).
    applied: usize,
    /// Diagnostics left after the final round — not mechanically fixable.
    remaining: usize,
}

fn fix_file(
    path: &Path,
    schema_dir: Option<&PathBuf>,
    dry_run: bool,
) -> Result<FixOutcome, String> {
    let original = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut text = original.clone();
    let mut applied = 0usize;
    let mut analysis = analyze(path, &text, schema_dir);

    for _ in 0..MAX_ROUNDS {
        let edits = sole_candidate_edits(&analysis.diags);
        if edits.is_empty() {
            break;
        }
        let candidate = match splice(&text, &edits) {
            Ok(c) => c,
            // Defense in depth: a batch the pre-filter let through but the
            // primitive refuses is a bug upstream, not a reason to write a
            // half-fixed file. Stop with what already passed re-check.
            Err(_) => break,
        };
        // Re-check before accepting: the round must strictly improve the
        // file at its own layer (see `improved`).
        let candidate_analysis = analyze(path, &candidate, schema_dir);
        if !improved(&analysis, &candidate_analysis) {
            break;
        }
        text = candidate;
        applied += edits.len();
        analysis = candidate_analysis;
    }

    if applied > 0 {
        if dry_run {
            print!("{}", unified_diff(&original, &text, path));
        } else {
            crate::write_file_atomically(&path.to_path_buf(), &text)?;
        }
    }
    Ok(FixOutcome {
        applied,
        remaining: analysis.diags.len(),
    })
}

/// One analysis of one text: parse totally; when the parse is clean, run
/// the same symbols + single-schema-universe + validation sequence
/// `check` runs. When the parse is NOT clean, the diagnostics are the
/// parse errors alone — validating an error-recovered AST would derive
/// fixes from guessed structure.
struct Analysis {
    parse_clean: bool,
    diags: Vec<Diagnostic>,
}

fn analyze(path: &Path, source: &str, schema_dir: Option<&PathBuf>) -> Analysis {
    let (file, parse_diags) = nml_core::cst::parse_to_ast_all(source);
    if !parse_diags.is_empty() {
        return Analysis {
            parse_clean: false,
            diags: parse_diags,
        };
    }

    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);
    diags.extend(symbols.find_duplicates());
    diags.extend(symbols.find_unresolved_references(&file));
    diags.extend(symbols.find_const_cycles());

    // The fixer only rewrites THIS file, so foreign-source findings are
    // context, not fix candidates — but a schema universe that fails to
    // assemble (I/O) degrades to fixing what the file alone shows.
    let file_name = path.display().to_string();
    let Ok(named_sources) = crate::pipeline::schema_universe(path, source, schema_dir) else {
        return Analysis {
            parse_clean: true,
            diags,
        };
    };
    let source_refs: Vec<(&str, &str)> = named_sources
        .iter()
        .map(|(n, _, t)| (n.as_str(), t.as_str()))
        .collect();
    let (schema, schema_diags) = nml_validate::loader::load_schema(&source_refs);
    let own_name = named_sources
        .iter()
        .find(|(_, p, _)| p == path)
        .map(|(n, _, _)| n.as_str())
        .unwrap_or(file_name.as_str());
    diags.extend(
        schema_diags
            .into_iter()
            .filter(|d| d.source.as_deref().is_none_or(|s| s == own_name)),
    );
    if !schema.is_empty() {
        diags.extend(
            SchemaValidator::from(schema)
                .composition_checked_at_load()
                .validate(&file),
        );
    }
    Analysis {
        parse_clean: true,
        diags,
    }
}

/// The re-check-and-revert rule, layer-aware. Crossing the parse →
/// validation boundary legitimately REVEALS diagnostics (a file that
/// finally parses gets validated for the first time), so a raw
/// total-count comparison would falsely revert exactly the most valuable
/// rounds:
///
/// * Parse layer dirty: the round must reduce the parse-error count;
///   reaching a clean parse is an improvement regardless of what
///   validation then finds.
/// * Parse layer clean: the round must keep it clean AND strictly reduce
///   the total count — a validation fix that breaks the parse or merely
///   reshuffles findings is discarded.
fn improved(before: &Analysis, after: &Analysis) -> bool {
    if !before.parse_clean {
        return after.parse_clean || after.diags.len() < before.diags.len();
    }
    after.parse_clean && after.diags.len() < before.diags.len()
}

/// The sole-candidate filter (module doc): one suggestion per diagnostic,
/// agreement across diagnostics per span, and a greedy non-overlap pass
/// (an overlapped candidate is simply deferred — the next round re-derives
/// it against the updated text).
///
/// **Structural-injection guard:** a replacement containing a line break
/// or any other control character is refused outright. Every suggestion
/// is a same-line token rewrite by design, but some replacements embed
/// *decoded user content* (the role-literal fix carries the string's
/// value), and a crafted escape sequence (`"admin\n evil = 1"`) would
/// otherwise let file content smuggle new lines — new *structure* —
/// through an auto-applied fix. Editors present quick-fixes for a human
/// to eyeball; a batch applier must refuse this class by construction.
fn sole_candidate_edits(diags: &[Diagnostic]) -> Vec<SpliceEdit> {
    let mut candidates: Vec<SpliceEdit> = diags
        .iter()
        .filter_map(|d| match d.suggestions.as_slice() {
            [one] if !one.replacement.chars().any(char::is_control) => Some(SpliceEdit {
                span: one.span,
                replacement: one.replacement.clone(),
            }),
            _ => None,
        })
        .collect();
    candidates.sort_by(|a, b| {
        (a.span.start, a.span.end, &a.replacement).cmp(&(b.span.start, b.span.end, &b.replacement))
    });
    candidates.dedup();

    // Two diagnostics proposing DIFFERENT texts for one span: neither is
    // the sole candidate — drop both rather than pick.
    let mut edits: Vec<SpliceEdit> = Vec::new();
    let mut i = 0;
    while i < candidates.len() {
        let same_span_end = candidates[i + 1..]
            .iter()
            .take_while(|c| c.span == candidates[i].span)
            .count()
            + i
            + 1;
        if same_span_end == i + 1 {
            edits.push(candidates[i].clone());
        }
        i = same_span_end;
    }

    // Greedy non-overlap, in offset order.
    let mut kept: Vec<SpliceEdit> = Vec::new();
    for e in edits {
        if kept.last().is_none_or(|prev| prev.span.end <= e.span.start) {
            kept.push(e);
        }
    }
    kept
}

/// A minimal unified diff (3 lines of context) for `--dry-run`. Line-based
/// LCS — fix targets are configuration files, small by nature; a
/// pathological pair falls back to one whole-file hunk rather than
/// quadratic work.
fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let mut out = format!("--- a/{0}\n+++ b/{0}\n", path.display());
    const CONTEXT: usize = 3;

    let ops = diff_ops(&old_lines, &new_lines);
    // Group ops into hunks separated by > 2*CONTEXT equal lines.
    let mut idx = 0;
    while idx < ops.len() {
        // Skip leading equals.
        while idx < ops.len() && matches!(ops[idx].2, OpKind::Equal) {
            idx += 1;
        }
        if idx == ops.len() {
            break;
        }
        let hunk_start = idx.saturating_sub(CONTEXT);
        // Extend through changes until a gap of > 2*CONTEXT equals.
        let mut end = idx;
        let mut gap = 0;
        let mut last_change = idx;
        while end < ops.len() && gap <= 2 * CONTEXT {
            if matches!(ops[end].2, OpKind::Equal) {
                gap += 1;
            } else {
                gap = 0;
                last_change = end;
            }
            end += 1;
        }
        let hunk_end = (last_change + 1 + CONTEXT).min(ops.len());

        let hunk = &ops[hunk_start..hunk_end];
        let old_start = hunk.first().map(|(o, _, _)| o + 1).unwrap_or(1);
        let new_start = hunk.first().map(|(_, n, _)| n + 1).unwrap_or(1);
        let old_count = hunk
            .iter()
            .filter(|(_, _, k)| !matches!(k, OpKind::Insert))
            .count();
        let new_count = hunk
            .iter()
            .filter(|(_, _, k)| !matches!(k, OpKind::Delete))
            .count();
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for (o, n, kind) in hunk {
            let (sigil, line) = match kind {
                OpKind::Equal => (' ', old_lines[*o]),
                OpKind::Delete => ('-', old_lines[*o]),
                OpKind::Insert => ('+', new_lines[*n]),
            };
            out.push(sigil);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
        idx = hunk_end;
    }
    out
}

#[derive(Clone, Copy)]
enum OpKind {
    Equal,
    Delete,
    Insert,
}

/// `(old_index, new_index, kind)` edit script via LCS. Indices are the
/// positions each op consumes (for `Insert`, `old_index` is where it
/// lands; for `Delete`, `new_index` likewise) — exactly what hunk
/// headers need.
fn diff_ops(old: &[&str], new: &[&str]) -> Vec<(usize, usize, OpKind)> {
    const MAX_CELLS: usize = 4_000_000;
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        // Fallback: one whole-file replacement.
        let mut ops: Vec<(usize, usize, OpKind)> = Vec::new();
        for i in 0..old.len() {
            ops.push((i, 0, OpKind::Delete));
        }
        for j in 0..new.len() {
            ops.push((old.len(), j, OpKind::Insert));
        }
        return ops;
    }
    // LCS lengths table.
    let mut lcs = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            ops.push((i, j, OpKind::Equal));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push((i, j, OpKind::Delete));
            i += 1;
        } else {
            ops.push((i, j, OpKind::Insert));
            j += 1;
        }
    }
    while i < old.len() {
        ops.push((i, j, OpKind::Delete));
        i += 1;
    }
    while j < new.len() {
        ops.push((i, j, OpKind::Insert));
        j += 1;
    }
    ops
}
