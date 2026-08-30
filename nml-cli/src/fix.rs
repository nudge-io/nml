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
//! * **One resolver for every applier**
//!   ([`nml_core::cst::edit::resolve_suggestions`], RFC 0023): verbatim
//!   substitution with the structural-injection refusal, structural
//!   deletions by token walks, batch overlap — and **every refusal is
//!   printed** (`fix refused: …`), never hidden behind
//!   "0 edit(s) applied".
//! * **Highest-offset-first splicing** ([`nml_core::cst::edit::splice`]),
//!   so earlier edits cannot invalidate later spans.
//! * **Re-check and revert.** Every round's result is re-analyzed before
//!   it is accepted: the parse layer must not regress, and for every
//!   `(code, message)` key the round applied a fix for, the count must
//!   drop by at least the number applied — a multiset decrement; a
//!   revealed finding lands on a key the round did not apply and is
//!   welcome. A failed round retries as its first applied candidate
//!   alone before the fixpoint is declared; a retry that still fails is
//!   a genuinely moved finding and reverts. The check runs on the
//!   in-memory candidate *before* any write — strictly safer than
//!   write-then-revert, with the same guarantee: a fixer that can worsen
//!   a file is worse than none. Writes go through the same atomic writer
//!   `fmt` uses.
//! * **Rounds to a fixpoint** (bounded): parse-layer fixes (`=>` → `->`)
//!   can unblock validation-layer fixes (`"30s"` → `30s`), which only
//!   become visible once the file parses; each round re-derives
//!   diagnostics from the current text. A round that resolves to zero
//!   edits ends the loop, its refusals printed.

use std::path::{Path, PathBuf};

use std::collections::{HashMap, HashSet};

use nml_core::cst::edit::{Resolved, resolve_suggestions, splice};
use nml_core::diagnostic::{Diagnostic, Suggestion};
use nml_core::layers::{FindingKey, finding_key};
use nml_validate::schema::SchemaValidator;

/// Floor on fix rounds per file: two layers (parse, then validation)
/// plus headroom for fixes that reveal fixes. The real budget scales
/// with the file ([`round_budget`]): plain same-message findings land
/// TOGETHER (the multiset decrement is per key, not per instance), but
/// a batch COLLIDES when another applied fix un-suppresses a
/// same-message finding — an NML2077 repair revealing an NML2060 whose
/// key the round also applied — and a colliding batch lands ONE
/// candidate per round; a fixed budget of eight stalled a fully
/// fixable mixed file.
const MIN_ROUNDS: usize = 8;

/// Ceiling on fix rounds per file — a bound on re-analysis work (each
/// round is one full re-analysis), not a convergence aid. A very wide
/// colliding file can reach it with edits still landing; the run says
/// so and a second `nml fix` continues from the fixpoint reached.
const MAX_ROUNDS: usize = 64;

/// The per-file round budget: one round per initial finding, plus the
/// reveal headroom, clamped to [`MIN_ROUNDS`]..=[`MAX_ROUNDS`].
fn round_budget(initial_findings: usize) -> usize {
    (initial_findings + MIN_ROUNDS).clamp(MIN_ROUNDS, MAX_ROUNDS)
}

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
    let mut exhausted_files = 0usize;
    for path in &files {
        let outcome = fix_file(path, schema_dir.as_ref(), dry_run)?;
        if outcome.budget_exhausted {
            exhausted_files += 1;
        }
        if outcome.applied > 0 {
            fixed_files += 1;
            total_edits += outcome.applied;
            let verb = if dry_run { "would fix" } else { "fixed" };
            // Walked filenames are repo content — sanitized like every
            // other surface that prints them.
            println!(
                "{verb} {} ({} edit(s))",
                crate::sanitized(&path.display().to_string()),
                outcome.applied
            );
        }
        remaining += outcome.remaining;
    }
    let noun = if dry_run { "appliable" } else { "applied" };
    // An exhausted file's remainder is NOT "not auto-fixable" — the
    // budget cut the run mid-landing; label it honestly.
    // Dry runs get their own tail: nothing was written, so "again"
    // would imply persisted progress that does not exist.
    let budget_note = match (exhausted_files, dry_run) {
        (0, _) => String::new(),
        (n, false) => {
            format!(" ({n} file(s) hit the round budget — run `nml fix` again to continue)")
        }
        (n, true) => {
            format!(" ({n} file(s) hit the round budget — a real run will need more than one pass)")
        }
    };
    println!(
        "{total_edits} edit(s) {noun} across {fixed_files} of {} file(s); {remaining} diagnostic(s) not auto-fixable{budget_note}",
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
    /// Diagnostics left after the final round. WITHOUT budget
    /// exhaustion these are not mechanically fixable; an exhausted file
    /// still holds landable candidates, and the summary says so.
    remaining: usize,
    /// The round budget ran out with sole candidates still standing —
    /// the remainder is not "not auto-fixable", another run continues.
    budget_exhausted: bool,
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
    // Each distinct refusal prints once per file — rounds re-derive
    // their candidates, and a persisting refusal would repeat.
    let mut printed: HashSet<String> = HashSet::new();

    let budget = round_budget(analysis.diags.len());
    let mut exhausted = true;
    for _ in 0..budget {
        let sole = sole_candidates(&analysis.diags);
        if sole.is_empty() {
            exhausted = false;
            break;
        }
        let suggestions: Vec<Suggestion> = sole.iter().map(|(_, s)| s.clone()).collect();
        let resolved = resolve_suggestions(&text, &suggestions);
        print_refusals(path, &text, &resolved, &mut printed);
        // A round that resolves to zero edits ends the loop.
        if resolved.edits.is_empty() {
            exhausted = false;
            break;
        }
        let Some((next, count, next_analysis)) =
            accept_round(path, schema_dir, &text, &analysis, &sole, &resolved)
        else {
            exhausted = false;
            break;
        };
        text = next;
        applied += count;
        analysis = next_analysis;
    }
    let budget_exhausted = exhausted && !sole_candidates(&analysis.diags).is_empty();
    if budget_exhausted {
        // Not a fixpoint — the budget ran out with sole candidates still
        // STANDING (never attempted; the next run derives and tries
        // them). Say so instead of mislabeling them "not auto-fixable".
        eprintln!(
            "{}: note: fix round budget reached with fix candidates still standing — \
             run `nml fix` again to continue",
            crate::sanitized(&path.display().to_string())
        );
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
        budget_exhausted,
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
    // Compose before validating (RFC 0019): the fixer must see the same
    // baseline `check` sees — validating a raw overlay body derives fixes
    // from a phantom (uncomposed) instance, and the compose diagnostics
    // themselves carry machine-applicable fixes (NML2060's deletion,
    // NML2077's remove-the-ref) that are unreachable without composing.
    let validator =
        (!schema.is_empty()).then(|| SchemaValidator::from(schema).composition_checked_at_load());
    let empty_index = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
    let index = validator.as_ref().map_or(&empty_index, |v| v.index());
    let composed =
        nml_core::layers::compose_file(index, own_name, &file, &nml_core::layers::OpenContext);
    // Foreign-source compose findings are context, not fix candidates —
    // same rule as schema_diags above.
    diags.extend(
        composed
            .diagnostics
            .into_iter()
            .filter(|d| d.source.as_deref().is_none_or(|s| s == own_name)),
    );
    if let Some(validator) = &validator {
        // One home per finding across the compose and validate passes —
        // a duplicated diagnostic would apply the same edit twice.
        let mut seen: std::collections::HashSet<nml_core::layers::FindingKey> =
            diags.iter().map(nml_core::layers::finding_key).collect();
        for diag in validator.validate(composed.validation_file.as_ref().unwrap_or(&file)) {
            if seen.insert(nml_core::layers::finding_key(&diag)) {
                diags.push(diag);
            }
        }
    }
    Analysis {
        parse_clean: true,
        diags,
    }
}

/// One round's acceptance: splice, re-analyze, gate ([`round_improved`]).
/// On a failed gate, retry the round as the FIRST APPLIED sole candidate
/// alone — the first in suggestion-span order whose outcome was `Ok`; a
/// refused candidate contributed no edits and cannot have failed the
/// gate — before the fixpoint is declared. A singleton that passes lands
/// and the next round re-derives the rest; a singleton that still fails
/// is a genuinely moved finding and reverts (visible as "not
/// auto-fixable").
fn accept_round(
    path: &Path,
    schema_dir: Option<&PathBuf>,
    text: &str,
    analysis: &Analysis,
    sole: &[(FindingKey, Suggestion)],
    resolved: &Resolved,
) -> Option<(String, usize, Analysis)> {
    let applied: Vec<&FindingKey> = resolved
        .outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| o.is_ok())
        .map(|(i, _)| &sole[i].0)
        .collect();
    if let Some(out) = try_edits(path, schema_dir, text, analysis, &applied, &resolved.edits) {
        return Some(out);
    }
    let first = resolved.outcomes.iter().position(|o| o.is_ok())?;
    let single = [sole[first].1.clone()];
    let retry = resolve_suggestions(text, &single);
    if retry.edits.is_empty() || retry.outcomes.first().is_none_or(|o| o.is_err()) {
        return None;
    }
    try_edits(
        path,
        schema_dir,
        text,
        analysis,
        &[&sole[first].0],
        &retry.edits,
    )
}

/// Splice, re-analyze, gate — `None` reverts the attempt. A batch the
/// resolver produced but the primitive refuses is a bug upstream, not a
/// reason to write a half-fixed file (defense in depth).
fn try_edits(
    path: &Path,
    schema_dir: Option<&PathBuf>,
    text: &str,
    analysis: &Analysis,
    applied: &[&FindingKey],
    edits: &[nml_core::cst::edit::SpliceEdit],
) -> Option<(String, usize, Analysis)> {
    let candidate = splice(text, edits).ok()?;
    let after = analyze(path, &candidate, schema_dir);
    round_improved(analysis, &after, applied).then_some((candidate, edits.len(), after))
}

/// The re-check gate. Two clauses:
///
/// * **The parse layer never regresses** (`after.parse_clean ||
///   !before.parse_clean`): reaching a clean parse is an improvement
///   regardless of what validation then finds — crossing the boundary
///   legitimately REVEALS diagnostics — and a validation fix that breaks
///   the parse is discarded.
/// * **A multiset decrement over the keys the round applied**: for every
///   `(code, message)` key with `applied(key) > 0`,
///   `count_after(key) <= count_before(key) − applied(key)`. Keys the
///   round did not apply are unconstrained — a revealed finding normally
///   lands on one (a raw count comparison rejected a round that reveals
///   as many findings as it fixes, sticking the file at a false
///   fixpoint; a gate over EVERY key would reject the reveal it exists
///   to accept; and a gate over keys present before would reject a
///   repair that reveals more instances of an existing key).
fn round_improved(before: &Analysis, after: &Analysis, applied: &[&FindingKey]) -> bool {
    if !(after.parse_clean || !before.parse_clean) {
        return false;
    }
    let mut applied_counts: HashMap<(Option<nml_core::diagnostic::Code>, &str), usize> =
        HashMap::new();
    for k in applied {
        *applied_counts.entry((k.0, k.2.as_str())).or_default() += 1;
    }
    let count = |diags: &[Diagnostic], key: &(Option<nml_core::diagnostic::Code>, &str)| {
        diags
            .iter()
            .filter(|d| d.code == key.0 && d.message == key.1)
            .count()
    };
    applied_counts
        .iter()
        .all(|(key, n)| count(&after.diags, key) + n <= count(&before.diags, key))
}

/// The sole-candidate filter (module doc): one suggestion per
/// diagnostic, byte-identical `(span, replacement, kind)` candidates
/// collapsed to one application, same-span disagreement dropping both,
/// sorted by suggestion span — the order the resolver's greedy batch
/// rules assume. Overlap, injection, and every structural concern belong
/// to the RESOLVER, where each refusal is per-suggestion and printed: a
/// silent pre-filter here would be an exemption from "every applier
/// refuses it, by construction, in one place". The paired
/// [`FindingKey`] is what the round gate decrements.
fn sole_candidates(diags: &[Diagnostic]) -> Vec<(FindingKey, Suggestion)> {
    let mut candidates: Vec<(FindingKey, Suggestion)> = diags
        .iter()
        .filter_map(|d| match d.suggestions.as_slice() {
            [one] => Some((finding_key(d), one.clone())),
            _ => None,
        })
        .collect();
    candidates.sort_by(|a, b| {
        (a.1.span.start, a.1.span.end, &a.1.replacement).cmp(&(
            b.1.span.start,
            b.1.span.end,
            &b.1.replacement,
        ))
    });
    // Two diagnostics carrying the same suggestion are ONE application.
    candidates.dedup_by(|a, b| a.1 == b.1);

    // Two diagnostics proposing DIFFERENT texts for one span: neither is
    // the sole candidate — drop both rather than pick.
    let mut out: Vec<(FindingKey, Suggestion)> = Vec::new();
    let mut i = 0;
    while i < candidates.len() {
        let same_span_end = candidates[i + 1..]
            .iter()
            .take_while(|c| c.1.span == candidates[i].1.span)
            .count()
            + i
            + 1;
        if same_span_end == i + 1 {
            out.push(candidates[i].clone());
        }
        i = same_span_end;
    }
    out
}

/// `<file>:<line>:<col>: fix refused: <reason>` — a refusal is a
/// legitimate outcome, not an upstream bug; hiding it behind
/// "0 edit(s) applied" left the user staring at an unexplained
/// fixpoint. Each distinct line prints once per file.
fn print_refusals(path: &Path, text: &str, resolved: &Resolved, printed: &mut HashSet<String>) {
    let map = nml_core::span::SourceMap::new(text);
    for outcome in &resolved.outcomes {
        let Err(e) = outcome else { continue };
        let loc = map.location(e.span().start);
        let msg = format!(
            "{}:{}:{}: fix refused: {e}",
            crate::sanitized(&path.display().to_string()),
            loc.line,
            loc.column
        );
        if printed.insert(msg.clone()) {
            eprintln!("{msg}");
        }
    }
}

/// A minimal unified diff (3 lines of context) for `--dry-run`. Line-based
/// LCS — fix targets are configuration files, small by nature; a
/// pathological pair falls back to one whole-file hunk rather than
/// quadratic work.
fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    let mut out = format!(
        "--- a/{0}\n+++ b/{0}\n",
        crate::sanitized(&path.display().to_string())
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::diagnostic::codes;
    use nml_core::span::Span;

    fn d(msg: &str) -> Diagnostic {
        Diagnostic::error(msg)
            .with_code(codes::SEALED_FIELD_VIOLATION)
            .with_span(Span::new(0, 1))
    }

    fn a(parse_clean: bool, msgs: &[&str]) -> Analysis {
        Analysis {
            parse_clean,
            diags: msgs.iter().map(|m| d(m)).collect(),
        }
    }

    fn key(msg: &str) -> FindingKey {
        (
            Some(codes::SEALED_FIELD_VIOLATION),
            Some((0, 1)),
            msg.to_string(),
        )
    }

    #[test]
    fn gate_accepts_an_exact_decrement() {
        let (before, after) = (a(true, &["k"]), a(true, &[]));
        assert!(round_improved(&before, &after, &[&key("k")]));
    }

    #[test]
    fn gate_accepts_a_reveal_on_an_unapplied_key() {
        // The false-fixpoint class: a round that reveals as many findings
        // as it fixes (the NML2077 → NML2060 probe) must land.
        let (before, after) = (a(true, &["fixed"]), a(true, &["revealed"]));
        assert!(round_improved(&before, &after, &[&key("fixed")]));
    }

    #[test]
    fn gate_accepts_a_reveal_of_more_instances_of_an_existing_key() {
        // A key present before but NOT applied this round is
        // unconstrained — a repaired ref can reveal more of it.
        let (before, after) = (a(true, &["fixed", "other"]), a(true, &["other", "other"]));
        assert!(round_improved(&before, &after, &[&key("fixed")]));
    }

    #[test]
    fn gate_rejects_a_surviving_applied_key() {
        // The compound-reveal class: an applied fix whose (code, message)
        // count did not drop — another applied fix un-suppressed an
        // identical-message finding — fails, and the caller retries the
        // first applied candidate alone.
        let (before, after) = (a(true, &["k"]), a(true, &["k"]));
        assert!(!round_improved(&before, &after, &[&key("k")]));
    }

    #[test]
    fn gate_accepts_one_of_two_message_identical_findings_applied() {
        let (before, after) = (a(true, &["k", "k"]), a(true, &["k"]));
        assert!(round_improved(&before, &after, &[&key("k")]));
    }

    #[test]
    fn gate_rejects_a_parse_regression_and_accepts_reaching_a_clean_parse() {
        let (before, after) = (a(true, &["k"]), a(false, &[]));
        assert!(
            !round_improved(&before, &after, &[&key("k")]),
            "a fix that breaks the parse is discarded"
        );
        let (before, after) = (a(false, &["k"]), a(true, &["x", "y", "z"]));
        assert!(
            round_improved(&before, &after, &[&key("k")]),
            "crossing into a clean parse legitimately reveals findings"
        );
    }
}
