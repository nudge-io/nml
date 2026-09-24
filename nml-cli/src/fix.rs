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
use nml_core::layers::{FindingKey, finding_key_in};
use nml_validate::schema::SchemaValidator;
use nml_validate::workspace::Resolved as Target;
use nml_validate::workspace::VocabularyMatch;

/// Floor on fix rounds per file: two layers (parse, then validation)
/// plus headroom for fixes that reveal fixes. The real budget scales
/// with the file ([`round_budget`]): plain same-message findings land
/// TOGETHER (the multiset decrement is per key, not per instance), but
/// a batch COLLIDES when another applied fix un-suppresses a
/// same-message finding — an NML2077 repair revealing an NML2060 whose
/// key the round also applied — and a colliding batch lands ONE
/// candidate per round; a fixed budget of eight stalled a fully
/// fixable mixed file.
///
/// LIMIT: reach=content guards=work surface=cli shown="8" — floor on fix rounds per file
const MIN_ROUNDS: usize = 8;

/// Ceiling on fix rounds per file — a bound on re-analysis work (each
/// round is one full re-analysis), not a convergence aid. A very wide
/// colliding file can reach it with edits still landing; the run says
/// so and a second `nml fix` continues from the fixpoint reached.
///
/// LIMIT: reach=content guards=work surface=cli shown="64" — fix rounds per file; a file that hits it says so and a second run continues
const MAX_ROUNDS: usize = 64;

/// Cells of the `--dry-run` diff's LCS table (old lines × new lines)
/// before it falls back to one whole-file hunk: fix targets are
/// configuration files, small by nature, and a pathological pair must
/// not buy quadratic work.
///
/// LIMIT: reach=content guards=work surface=cli shown="4000000" — cells in the `--dry-run` diff matrix before it falls back to a coarser diff
const MAX_CELLS: usize = 4_000_000;

/// The per-file round budget: one round per initial finding, plus the
/// reveal headroom, clamped to [`MIN_ROUNDS`]..=[`MAX_ROUNDS`].
fn round_budget(initial_findings: usize) -> usize {
    (initial_findings + MIN_ROUNDS).clamp(MIN_ROUNDS, MAX_ROUNDS)
}

/// What `nml fix` accepts, and its page (`crate::Spec`).
pub(crate) const SPEC: crate::Spec = crate::Spec {
    verb: "fix",
    summary: "Apply machine-applicable fixes (migrations, sole-candidate suggestions) in \
              bulk; directories are walked for .nml files; every refusal is printed.",
    root: true,
    schema: true,
    strict: false,
    edit: Some(crate::Edit::Fix),
    max_findings: true,
    list: false,
    targets: "<path>...",
    arity: crate::Arity::Many,
    exits: &[
        (
            "0",
            "every path was fixed or is clean (a warning-only remainder included)",
        ),
        (
            "1",
            "a path could not be fixed — absent, unreadable, or refused by the universe (NML2083, NML2087, NML2089) — a universe error, a directory naming no .nml file, or, under --check, any fix that would apply, any error no fix repairs, or .nml content a directory walk skipped (NML2090)",
        ),
        (
            "2",
            "a usage error, or --schema beside a manifest-governed file",
        ),
    ],
    examples: &[
        "nml fix --check --root . .        # CI: fail when the tree is unfixed",
        "nml fix --dry-run --root . .      # show the diff, write nothing, exit 0",
        "nml fix --root . tenants/cu/      # apply the fixes under one tenant",
    ],
};

pub fn cmd_fix(args: &[String]) -> Result<(), String> {
    let inv = crate::parse_invocation(args, &SPEC)?;
    let (schema_dir, dry_run, gate) = (inv.schema.clone(), inv.dry_run, inv.check);
    if let Some(dir) = &schema_dir {
        crate::workspace::require_schema_dir(dir)?;
    }
    // The closing row carries `fix`'s fields on EVERY path — a run that
    // finds no `.nml` file included: a consumer reads zeros, never
    // absences. Recorded before the door, replaced by the tally after.
    set_fix_fields(dry_run, &FixTally::default());
    // One universe per invocation (RFC 0019 item 0), through the door
    // every workspace verb uses: the root is fixed once — `--root`, else
    // derived from the FIRST path argument — a universe that cannot be
    // trusted rewrites NOTHING (A16, E28: the fixer would otherwise judge
    // every file under it as unbound and repair against the wrong, or
    // no, vocabulary — refused BEFORE any argument is expanded, E35),
    // and every directory argument expands to the kernel's enumeration
    // below the root (E32).
    let ws = crate::workspace::Workspace::open(inv.root.as_deref(), &inv.targets)?;
    let expanded = match crate::admit_targets(&ws, &inv) {
        Ok(expanded) => expanded,
        // The door refused (a manifest failed to load): its rows are
        // printed and nothing is rewritten — and the edits those rows
        // carry in the universe's own inputs (a failed manifest's
        // did-you-mean, riding its NML2088 row) are pending THERE, so the
        // closing row's `routed` says so: a consumer tells "no fix
        // exists" from "the fix is not this run's" at the door as after
        // a round.
        Err(refused) => {
            let routed = routed_by_universe(&ws);
            set_fix_fields(
                dry_run,
                &FixTally {
                    routed,
                    ..Default::default()
                },
            );
            // The VERDICT, in the same words the run would have closed
            // with after a round: the rows above are the findings, and
            // this says what became of them — nothing written, and the
            // repair not this run's to make. Without it the human run
            // ended at `error: N error(s)`, the one surface of four that
            // did not say where the edit is pending. Silent under `-q`
            // (errors only), as every closing tally is; `--json` says it
            // as `routed` on the closing row.
            // ONLY where rows stand above it: the door also refuses a
            // target naming no `.nml` file, and there the verdict blamed
            // "the error(s) above" when the reader had seen none — the
            // `error:` line is then the whole story.
            if !crate::out::json() && !crate::out::quiet() && crate::reported_a_coded_finding() {
                let pending = if routed > 0 {
                    format!(
                        " ({routed} edit(s) those findings carry are pending in the \
                         universe's own inputs — take the did-you-mean or paste the \
                         `help:` block `nml check` shows there, or apply the editor's \
                         quick fix)"
                    )
                } else {
                    String::new()
                };
                crate::out::say(format_args!(
                    "nothing was written: the universe validates and rewrites nothing \
                     until the error(s) above are repaired{pending}"
                ));
            }
            return Err(refused);
        }
    };
    let files: Vec<PathBuf> = expanded.files.iter().map(PathBuf::from).collect();
    // The gate (`--check`) also fails on content the walk skipped under a
    // directory argument: a walked symlinked `.nml` used to
    // pass with exit 0 while the same link, named, was NML2083.
    let unjudged = if gate {
        crate::report_unjudged(&ws, &expanded.dirs)
    } else {
        0
    };

    let mut fixed_files = 0usize;
    let mut total_edits = 0usize;
    let mut remaining = 0usize;
    let mut remaining_errors = 0usize;
    let mut suppressed_total = 0usize;
    let mut exhausted_files = 0usize;
    let mut failed_files = 0usize;
    let mut routed = 0usize;
    for path in &files {
        // A bad argument — outside the root, unreadable, absent — fails
        // ITS OWN file through the shared reporter and the run continues
        // (E35): `fix a /etc/hosts b` fixes `a` and `b`, reports the
        // middle one, prints the summary, and exits 1 for the failure.
        let outcome = match fix_file(path, &ws, schema_dir.as_ref(), dry_run) {
            Ok(outcome) => outcome,
            Err(e) => {
                crate::out::error_line(&e, 1, "target");
                failed_files += 1;
                continue;
            }
        };
        // A path the universe REFUSED — a closed binding's rejection
        // (NML2083), an ambiguous claim (NML2087), a denied unit
        // (NML2089) — could not be fixed, exactly as an absent path could
        // not: its finding printed above, the run goes on and exits 1
        // for it, so `set -e` sees what the terminal shows. `--check`
        // stays the gate for the pending edits.
        if outcome.refused {
            failed_files += 1;
        }
        if let Some(diff) = &outcome.diff {
            if !crate::out::json() {
                crate::out::say_raw(diff);
            }
        }
        if outcome.budget_exhausted {
            exhausted_files += 1;
        }
        if outcome.applied > 0 {
            fixed_files += 1;
            total_edits += outcome.applied;
            let verb = if dry_run { "would fix" } else { "fixed" };
            // Walked filenames are repo content — sanitized like every
            // other surface that prints them.
            if crate::out::json() {
                crate::out::emit(&serde_json::json!({
                    "type": "fix",
                    "file": path.display().to_string(),
                    "applied": outcome.applied,
                    "remaining": outcome.remaining,
                    "routed": outcome.routed,
                    "dryRun": dry_run,
                    "diff": outcome.diff,
                }));
            } else if !crate::out::quiet() {
                // Success output: silent under `-q`, as `check`'s `ok`
                // lines are (the diff above is the verb's answer and
                // stays).
                crate::out::say(format_args!(
                    "{verb} {} ({} edit(s))",
                    crate::sanitized(&path.display().to_string()),
                    outcome.applied
                ));
            }
        }
        remaining += outcome.remaining;
        remaining_errors += outcome.remaining_errors;
        suppressed_total += outcome.suppressed;
        routed += outcome.routed;
    }
    let noun = if dry_run { "would apply" } else { "applied" };
    // Suppressed findings are UNKNOWNS — never folded into the
    // not-auto-fixable count, only disclosed beside it. The
    // parenthetical binds to the NUMBER it qualifies (before the
    // pointer tail), speaks the marker row's vocabulary ("suppressed",
    // "limit" — never the numeric limit, which is per-file), and
    // prints on dry runs too: it qualifies the count, which prints
    // there; only the pointer below is dry-run-gated.
    let suppressed_note = if suppressed_total > 0 {
        format!(" ({suppressed_total} more suppressed past the diagnostic limit)")
    } else {
        String::new()
    };
    // A routed edit is not "no fix": it is a fix that lies in another
    // file (the refusal above names it) — said beside the count so an
    // operator reading the tally knows a change is pending elsewhere.
    let routed_note = if routed > 0 {
        format!(
            " ({routed} edit(s) belong to another file — pending there; `nml fix` never edits another file)"
        )
    } else {
        String::new()
    };
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
    // Standing diagnostics deserve a next action, not a dead end: some
    // carry enumerated ALTERNATIVES a human must pick from (the editor
    // offers each; `nml check` prints them), the rest carry no machine
    // repair at all. Suppressed on a dry run: nothing was applied, so
    // `nml check` would list the would-be-fixed findings too and "to
    // see them" would point at the wrong set.
    let standing_note = if remaining > 0 && !dry_run {
        " — run `nml check` to see them"
    } else {
        ""
    };
    crate::out::set_targets(files.len());
    set_fix_fields(
        dry_run,
        &FixTally {
            edits: total_edits,
            files_fixed: fixed_files,
            files: files.len(),
            remaining,
            routed,
            suppressed: suppressed_total,
            budget_exhausted: exhausted_files,
            failed: failed_files,
        },
    );
    if crate::out::json() {
        crate::out::mark_verdict();
    } else if !crate::out::quiet() {
        // The closing tally is success output too: silent under `-q`;
        // the gate's `error:` lines below are not.
        crate::out::say(format_args!(
            "{total_edits} edit(s) {noun} across {fixed_files} of {} file(s); {remaining} diagnostic(s) not auto-fixable{routed_note}{suppressed_note}{standing_note}{budget_note}",
            files.len()
        ));
    }
    if failed_files > 0 {
        return Err(format!("{failed_files} path(s) could not be fixed"));
    }
    // `--check` is the CI GATE, spelled as rustfmt, black, prettier and
    // gofmt spell it: nothing was written, and pending edits are the
    // failure — and so is an error-severity finding no fix repairs (a
    // file that does not parse, a planted symlink under a closed binding
    // and a unit-denied file must not pass a "CI gate" with exit 0;
    // rustfmt and prettier both fail `--check` on a file they cannot
    // parse).
    // `--dry-run` keeps its Unix meaning — "show me what would happen",
    // exit 0 — because a script that pipes a dry run into review
    // tooling under `set -e` must not start failing the day its tree
    // grows a fixable finding; and `fix` WITHOUT `--check` exits 0 on
    // standing findings no fix repairs (a refused PATH is the one
    // exception: it could not be fixed, exit 1, as an absent path), so
    // only the gate is a gate on the edits.
    if gate && (total_edits > 0 || remaining_errors > 0 || unjudged > 0) {
        let edits = (total_edits > 0).then(|| {
            format!(
                "{total_edits} fix(es) would apply across {fixed_files} file(s) — run `nml \
                 fix` to apply them"
            )
        });
        let errors = (remaining_errors > 0).then(|| {
            format!(
                "{remaining_errors} error(s) remain that no fix repairs — run `nml check` to \
                 see them"
            )
        });
        let elsewhere = (routed > 0)
            .then(|| format!("{routed} edit(s) belong to another file and are pending there"));
        let skipped = (unjudged > 0)
            .then(|| format!("{unjudged} skipped path(s) hold content no verb judged"));
        let why: Vec<String> = edits
            .into_iter()
            .chain(errors)
            .chain(elsewhere)
            .chain(skipped)
            .collect();
        return Err(format!("{}; nothing was written", why.join("; ")));
    }
    Ok(())
}

/// The run's tallies as the closing row carries them.
#[derive(Default)]
struct FixTally {
    edits: usize,
    files_fixed: usize,
    files: usize,
    remaining: usize,
    routed: usize,
    suppressed: usize,
    budget_exhausted: usize,
    failed: usize,
}

/// `fix`'s own fields ride the run's ONE closing row instead of a
/// second, verb-shaped terminal row — on every path, a run that found
/// nothing to fix included.
fn set_fix_fields(dry_run: bool, tally: &FixTally) {
    crate::out::set_summary_extra(vec![
        ("dryRun".to_string(), serde_json::json!(dry_run)),
        ("edits".to_string(), serde_json::json!(tally.edits)),
        (
            "filesFixed".to_string(),
            serde_json::json!(tally.files_fixed),
        ),
        ("files".to_string(), serde_json::json!(tally.files)),
        ("remaining".to_string(), serde_json::json!(tally.remaining)),
        ("routed".to_string(), serde_json::json!(tally.routed)),
        (
            "suppressed".to_string(),
            serde_json::json!(tally.suppressed),
        ),
        (
            "budgetExhausted".to_string(),
            serde_json::json!(tally.budget_exhausted),
        ),
        ("failed".to_string(), serde_json::json!(tally.failed)),
    ]);
}

struct FixOutcome {
    /// Edits applied (or, dry-run, that would be).
    applied: usize,
    /// Diagnostics left after the final round. WITHOUT budget
    /// exhaustion these are not mechanically fixable; an exhausted file
    /// still holds landable candidates, and the summary says so.
    remaining: usize,
    /// The error-severity part of `remaining` — what `--check` gates on
    /// besides pending edits: a parse error, a kernel refusal
    /// (NML2083, NML2089), a finding no fix repairs.
    remaining_errors: usize,
    /// The exact count the final analysis reported as suppressed past
    /// the diagnostic limit — findings never materialized, hence never
    /// CLASSIFIED: they are unknowns, and are never folded into
    /// `remaining` (which counts judged, standing findings only).
    suppressed: usize,
    /// The round budget ran out with sole candidates still standing —
    /// the remainder is not "not auto-fixable", another run continues.
    budget_exhausted: bool,
    /// Distinct edits standing after the final round that lie in
    /// ANOTHER file — refused here, pending there (an operator's change
    /// pending in the manifest). Part of `remaining`'s findings, named
    /// apart so a consumer tells "no fix exists" from "the fix is not
    /// this file's".
    routed: usize,
    /// The dry-run diff, returned to the caller (printed there as text,
    /// or carried in the JSON `fix` row).
    diff: Option<String>,
    /// The universe refused the path before it was opened (NML2083,
    /// NML2087, NML2089): its finding printed, nothing was fixed — the
    /// path "could not be fixed", as an absent one.
    refused: bool,
}

fn fix_file(
    path: &Path,
    ws: &crate::workspace::Workspace,
    schema_dir: Option<&PathBuf>,
    dry_run: bool,
) -> Result<FixOutcome, String> {
    // Resolve BEFORE reading (RFC 0019 item 0): a path a closed binding
    // rejects (NML2083) is reported and never opened; `--schema` beside a
    // governing binding is a usage error, as in `check`. The universe's
    // word on the file — inert inputs on its chain (NML2080) and the
    // kernel's findings — prints through the SAME reporter every verb
    // uses (E35: `fix` printed the errors and swallowed the notes).
    let resolved = ws.resolve(path)?;
    let error_count = crate::report_universe(&path.display().to_string(), ws, &resolved);
    if error_count > 0 {
        // A refused path's rows can carry a remedy in ANOTHER file —
        // NML2091 carries its declared source's deletion — and this run
        // rewrites nothing anywhere. The closing row says where those
        // edits are pending, by the same unit and the same door the
        // manifest's own remedy is tallied with (`routed_by_universe`):
        // the rows just printed, the edits that lie in another file.
        let own = resolved.key.as_str();
        let inert = ws.discovery().inert_notes_for(&resolved.key);
        let routed = distinct_edits(inert.iter().chain(&resolved.findings).flat_map(|d| {
            d.suggestions.iter().filter_map(move |s| {
                d.suggestion_source(s)
                    .filter(|src| *src != own)
                    .map(|src| (src, s))
            })
        }));
        // Rendered exactly as `check` renders a locationless finding —
        // severity, code, the run's explain hint — so the rejection the
        // fixer refuses reads as the diagnostic the gate reports; the
        // path could not be fixed (exit 1, as an absent path).
        return Ok(FixOutcome {
            applied: 0,
            remaining: error_count,
            remaining_errors: error_count,
            suppressed: 0,
            budget_exhausted: false,
            routed,
            diff: None,
            refused: true,
        });
    }
    // An absent argument, as the kernel saw it (no leaf at the minted
    // key): its own failure, never an OS error from a read.
    if resolved.kind.is_none() {
        return Err(format!("{}: no such file or directory", path.display()));
    }
    // A directory the walk did not enter (behind a link the open
    // universe follows): refused as `check` refuses it, never read.
    crate::refuse_directory(&resolved, path)?;
    // Judged ONCE per file (E35): the binding's validator is built here
    // and every round analyzes against it — `check` builds once, so does
    // `fix` (a round that re-resolved, re-judged and rebuilt the
    // validator from source would pay the judgement N times).
    let validator = match crate::workspace::judge(&resolved, schema_dir.map(PathBuf::as_path), path)
    {
        Ok(v) => v,
        Err(crate::workspace::Conflict(message)) => crate::exit_usage(&message),
    };
    let vocabulary = ws.discovery().vocabulary_for(&resolved.key).covered();
    let ctx = FixContext {
        path,
        schema_dir,
        resolved: &resolved,
        validator: validator.as_deref(),
        vocabulary: vocabulary.as_ref(),
    };
    // Resolved ONCE: the leaf the read came from is the leaf the
    // write lands on.
    let crate::workspace::Opened {
        text: original,
        leaf,
    } = ws.open_target(&resolved, path)?;
    let mut text = original.clone();
    let mut applied = 0usize;
    let mut analysis = analyze(&ctx, &text);
    // Each distinct refusal prints once per file — rounds re-derive
    // their candidates, and a persisting refusal would repeat.
    let mut printed: HashSet<String> = HashSet::new();

    let budget = round_budget(analysis.diags.len());
    let mut exhausted = true;
    for _ in 0..budget {
        let (sole, elsewhere) = sole_candidates(&analysis);
        print_elsewhere(path, &text, &elsewhere, &mut printed);
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
            accept_round(&ctx, &text, &analysis, &sole, &resolved)
        else {
            exhausted = false;
            break;
        };
        text = next;
        applied += count;
        analysis = next_analysis;
    }
    let (standing, elsewhere) = sole_candidates(&analysis);
    let budget_exhausted = exhausted && !standing.is_empty();
    let routed = distinct_edits(elsewhere.iter().map(|e| (e.source.as_str(), &e.edit)));
    if budget_exhausted {
        // Not a fixpoint — the budget ran out with sole candidates still
        // STANDING (never attempted; the next run derives and tries
        // them). Say so instead of mislabeling them "not auto-fixable".
        let msg = "fix round budget reached with fix candidates still standing — \
                   run `nml fix` again to continue";
        if crate::out::json() {
            crate::out::emit(&crate::out::info_value(
                &path.display().to_string(),
                None,
                msg,
            ));
        } else {
            crate::out::err(format_args!(
                "{}: {} {msg}",
                crate::sanitized(&path.display().to_string()),
                crate::out::paint(crate::out::Level::Note, "note:")
            ));
        }
    }

    let mut diff = None;
    if applied > 0 {
        if dry_run {
            diff = Some(unified_diff(&original, &text, path));
        } else {
            ws.write_target(&resolved, &leaf, path, &text)?;
        }
    }
    Ok(FixOutcome {
        diff,
        applied,
        // Info rows are advisories (the truncation marker among them),
        // not standing findings; warnings ARE genuine findings, so the
        // filter is not-Info, never Error-only.
        remaining: analysis
            .diags
            .iter()
            .filter(|d| d.severity != nml_core::diagnostic::Severity::Info)
            .count(),
        remaining_errors: analysis
            .diags
            .iter()
            .filter(|d| d.severity == nml_core::diagnostic::Severity::Error)
            .count(),
        suppressed: analysis.suppressed,
        budget_exhausted,
        routed,
        refused: false,
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
    /// The analyzed file's source NAME — the key vocabulary every
    /// finding of this analysis is read in (`finding_key_in`): an
    /// unstamped finding is a finding about this file.
    own: String,
    /// The exact count the parse reported as suppressed past the
    /// diagnostic cap (`nml_core::cst::suppressed_count` on the parse
    /// findings; 0 on a clean parse) — the Σ-deficit budget of
    /// [`round_improved`]: findings the cap HID can legitimately
    /// surface on an applied key without failing the round.
    suppressed: usize,
}

/// One file's fix context, settled ONCE per file and shared by every
/// round (E35): the path, the `--schema` flag, the file's resolution
/// under the universe (its key, its grant) and the binding's validator
/// when one governs it. `analyze` cannot re-resolve, re-judge the grant
/// or rebuild the binding's validator — it has no way to.
#[derive(Clone, Copy)]
struct FixContext<'a> {
    path: &'a Path,
    schema_dir: Option<&'a PathBuf>,
    resolved: &'a Target<'a>,
    validator: Option<&'a SchemaValidator>,
    /// The directive vocabulary covering a schema source (the kernel's
    /// answer, resolved once per file like the validator) — its
    /// did-you-mean is a fix candidate.
    vocabulary: Option<&'a VocabularyMatch>,
}

fn analyze(ctx: &FixContext<'_>, source: &str) -> Analysis {
    let FixContext {
        path, schema_dir, ..
    } = *ctx;
    // The key IS the file's name on the wire (step 0f).
    let file_name = ctx.resolved.key.to_string();
    // ONE parse per round: the extraction feeds the loader
    // below in place of the text — the same seam `check` uses.
    let (file, own_schema, parse_diags, facet_diags) =
        nml_core::cst::parse_and_extract_split(source);
    if !parse_diags.is_empty() {
        return Analysis {
            parse_clean: false,
            suppressed: nml_core::cst::suppressed_count(&parse_diags),
            diags: parse_diags,
            own: file_name,
        };
    }

    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);
    diags.extend(symbols.find_unresolved_references(&file));
    diags.extend(symbols.find_const_cycles());
    // The covering package's directive verdicts (the kernel's judge, shared
    // with `check` and the editor) — a did-you-mean among them is a fix
    // candidate; judged on the file's own extraction, before the load.
    if let Some(vocab) = ctx.vocabulary {
        diags.extend(vocab.judge(&own_schema.models, source));
    }

    // The fixer only rewrites THIS file, so foreign-source findings are
    // context, not fix candidates — but a schema universe that fails to
    // assemble (I/O) degrades to fixing what the file alone shows.
    let Ok(named_sources) = crate::workspace::schema_universe(path, &file_name, source, schema_dir)
    else {
        return Analysis {
            parse_clean: true,
            diags,
            suppressed: 0,
            own: file_name,
        };
    };
    let (schema, schema_diags) = nml_validate::loader::load_schema_parts(
        crate::workspace::schema_parts(&named_sources, source, (own_schema, facet_diags)),
    );
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
    // The universe judges here exactly as in `check`: a claim-governed
    // file validates under its binding's package (D-0d-1; the conflict
    // was settled before the first read, the validator built once per
    // file), and the grant provider is the shared resolution core. A
    // file no binding governs validates against the schema universe the
    // current text assembles (the file's own definitions change per
    // round, so that one is legitimately per round). The grant is the
    // resolution's own (`Resolved::grant`), settled once per file.
    let own_validator;
    let validator: Option<&SchemaValidator> = match ctx.validator {
        Some(v) => Some(v),
        None => {
            own_validator = (!schema.is_empty())
                .then(|| SchemaValidator::from(schema).composition_checked_at_load());
            own_validator.as_ref()
        }
    };
    let empty_index = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
    let index = validator.map_or(&empty_index, |v| v.index());
    let composed = nml_core::layers::compose_file(index, own_name, &file, &ctx.resolved.grant);
    // The composed findings' keys the validator's must meet on one key
    // — the kernel's rule (`ComposedFile::dedup_seed`): only while the
    // file composes, as `check` and the editor seed it.
    let seed = composed.dedup_seed(own_name);
    // Foreign-source compose findings are context, not fix candidates —
    // same rule as schema_diags above.
    diags.extend(
        composed
            .diagnostics
            .into_iter()
            .filter(|d| d.source.as_deref().is_none_or(|s| s == own_name)),
    );
    if let Some(validator) = validator {
        // One home per finding across the compose and validate passes —
        // a duplicated diagnostic would apply the same edit twice —
        // through THE deduplicating sink every front end validates a
        // composed file through (`layers::Deduped`), never a set of
        // this verb's own.
        let mut deduped = nml_core::layers::Deduped::new(&mut diags, own_name, seed);
        validator.validate_into(
            composed.validation_file.as_ref().unwrap_or(&file),
            &mut deduped,
        );
    }
    Analysis {
        parse_clean: true,
        diags,
        suppressed: 0,
        own: own_name.to_string(),
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
    ctx: &FixContext<'_>,
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
    if let Some(out) = try_edits(ctx, text, analysis, &applied, &resolved.edits) {
        return Some(out);
    }
    let first = resolved.outcomes.iter().position(|o| o.is_ok())?;
    let single = [sole[first].1.clone()];
    let retry = resolve_suggestions(text, &single);
    if retry.edits.is_empty() || retry.outcomes.first().is_none_or(|o| o.is_err()) {
        return None;
    }
    try_edits(ctx, text, analysis, &[&sole[first].0], &retry.edits)
}

/// Splice, re-analyze, gate — `None` reverts the attempt. A batch the
/// resolver produced but the primitive refuses is a bug upstream, not a
/// reason to write a half-fixed file (defense in depth).
fn try_edits(
    ctx: &FixContext<'_>,
    text: &str,
    analysis: &Analysis,
    applied: &[&FindingKey],
    edits: &[nml_core::cst::edit::SpliceEdit],
) -> Option<(String, usize, Analysis)> {
    let candidate = splice(text, edits).ok()?;
    let after = analyze(ctx, &candidate);
    round_improved(analysis, &after, applied).then_some((candidate, edits.len(), after))
}

/// The re-check gate. Two clauses:
///
/// * **The parse layer never regresses** (`after.parse_clean ||
///   !before.parse_clean`): reaching a clean parse is an improvement
///   regardless of what validation then finds — crossing the boundary
///   legitimately REVEALS diagnostics — and a validation fix that breaks
///   the parse is discarded.
/// * **A Σ-deficit multiset decrement over the keys the round applied**:
///   for every `(code, message)` key with `applied(key) > 0`, the key's
///   *deficit* is `(count_after(key) + applied(key)) −
///   count_before(key)`, floored at zero, and the deficits SUM to at
///   most `before.suppressed` — the exact count the diagnostic cap hid
///   (D-A). A capped flood (129 same-message findings, 128 visible)
///   legitimately re-surfaces its hidden instances on the very key the
///   round applied; the budget admits exactly those, and nothing else:
///   with nothing suppressed the sum must be zero, which is the
///   original per-key decrement exactly. The per-key floor means an
///   over-delivering key (count dropped by more than applied) earns no
///   credit — a failing key can never borrow another's slack. Keys the
///   round did not apply stay unconstrained — a revealed finding
///   normally lands on one (a raw count comparison rejected a round
///   that reveals as many findings as it fixes, sticking the file at a
///   false fixpoint; a gate over EVERY key would reject the reveal it
///   exists to accept; and a gate over keys present before would reject
///   a repair that reveals more instances of an existing key).
fn round_improved(before: &Analysis, after: &Analysis, applied: &[&FindingKey]) -> bool {
    if !(after.parse_clean || !before.parse_clean) {
        return false;
    }
    // Keyed by (code, message, source) — the span is what the fix moved,
    // so it is excluded; the SOURCE stays (a same-text finding in another
    // file is another finding, and its count is not this key's to spend).
    type GateKey<'k> = (Option<nml_core::diagnostic::Code>, &'k str, &'k str);
    let mut applied_counts: HashMap<GateKey<'_>, usize> = HashMap::new();
    for k in applied {
        let source = k.3.as_deref().unwrap_or(&before.own);
        *applied_counts
            .entry((k.0, k.2.as_str(), source))
            .or_default() += 1;
    }
    let count = |diags: &[Diagnostic], key: &GateKey<'_>| {
        diags
            .iter()
            .filter(|d| {
                d.code == key.0
                    && d.message == key.1
                    && d.source.as_deref().unwrap_or(&before.own) == key.2
            })
            .count()
    };
    let deficit: usize = applied_counts
        .iter()
        .map(|(key, n)| (count(&after.diags, key) + n).saturating_sub(count(&before.diags, key)))
        .sum();
    deficit <= before.suppressed
}

/// A sole suggestion whose edit lies in ANOTHER file
/// (`Diagnostic::suggestion_source`): a content file's finding may carry
/// the block an operator adds to the manifest. It is never resolved
/// against this file's text — a byte range means nothing across files,
/// and a name that happened to sit at the same offset here would take
/// the edit — and it is printed as this file's refusal, naming where the
/// edit goes.
struct Elsewhere {
    at: Option<nml_core::span::Span>,
    source: String,
    /// The edit itself — two findings that carry ONE edit in another
    /// file (two `uses` clauses, one binding to grant) are one routed
    /// edit, as two findings carrying one edit here are one application.
    edit: Suggestion,
}

/// The sole-candidate filter (module doc): one suggestion per
/// diagnostic, an edit in another file set aside ([`Elsewhere`]),
/// byte-identical `(span, replacement, kind)` candidates collapsed to
/// one application, same-span disagreement dropping both, sorted by
/// suggestion span — the order the resolver's greedy batch rules
/// assume. Overlap, injection, and every structural concern belong to
/// the RESOLVER, where each refusal is per-suggestion and printed: a
/// silent pre-filter here would be an exemption from "every applier
/// refuses it, by construction, in one place". The paired
/// [`FindingKey`] is what the round gate decrements.
fn sole_candidates(analysis: &Analysis) -> (Vec<(FindingKey, Suggestion)>, Vec<Elsewhere>) {
    let mut elsewhere: Vec<Elsewhere> = Vec::new();
    let mut candidates: Vec<(FindingKey, Suggestion)> = analysis
        .diags
        .iter()
        .filter_map(|d| match d.suggestions.as_slice() {
            [one] => match d.suggestion_source(one) {
                Some(source) if source != analysis.own => {
                    elsewhere.push(Elsewhere {
                        at: d.span,
                        source: source.to_string(),
                        edit: one.clone(),
                    });
                    None
                }
                _ => Some((finding_key_in(d, &analysis.own), one.clone())),
            },
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
    (out, elsewhere)
}

/// Distinct `(file, edit)` pairs — the `routed` count's unit: two
/// findings carrying ONE edit in another file are one routed edit, as
/// two findings carrying one edit here are one application.
fn distinct_edits<'a>(edits: impl Iterator<Item = (&'a str, &'a Suggestion)>) -> usize {
    let mut edits: Vec<(&str, &Suggestion)> = edits.collect();
    edits.sort_by(|a, b| {
        (a.0, a.1.span.start, a.1.span.end, &a.1.replacement).cmp(&(
            b.0,
            b.1.span.start,
            b.1.span.end,
            &b.1.replacement,
        ))
    });
    edits.dedup();
    edits.len()
}

/// The distinct edits the universe's rows carry in its own inputs — a
/// failed manifest's did-you-mean riding its NML2088 row
/// (`Diagnostic::caused_by`) — tallied as `routed` when the door
/// refuses the run: pending in the manifest, applied by the editor's
/// quick fix or by hand, never by a run under a universe that failed
/// to load.
fn routed_by_universe(ws: &crate::workspace::Workspace) -> usize {
    let notes = ws.universe_notes();
    distinct_edits(notes.iter().flat_map(|d| {
        d.suggestions
            .iter()
            .filter_map(move |s| d.suggestion_source(s).map(|src| (src, s)))
    }))
}

/// `<file>:<line>:<col>: fix refused: the edit is in <source> — …` for
/// each sole suggestion whose edit lies in another file: printed like
/// every refusal, once per file, so the reader learns where the fix
/// goes instead of staring at an unexplained fixpoint.
fn print_elsewhere(
    path: &Path,
    text: &str,
    elsewhere: &[Elsewhere],
    printed: &mut HashSet<String>,
) {
    let map = nml_core::span::SourceMap::new(text);
    for e in elsewhere {
        let loc = e.at.map(|span| {
            let loc = map.location(span.start);
            (loc.line, loc.column)
        });
        let reason = format!(
            "fix refused: the edit is in {} — `nml fix` never edits another file; take the did-you-mean or paste the `help:` block `nml check` shows there, or apply the editor's quick fix",
            e.source
        );
        let msg = match loc {
            Some((line, column)) => format!(
                "{}:{line}:{column}: {reason}",
                crate::sanitized(&path.display().to_string())
            ),
            None => format!(
                "{}: {reason}",
                crate::sanitized(&path.display().to_string())
            ),
        };
        if printed.insert(msg.clone()) {
            if crate::out::json() {
                crate::out::emit(&crate::out::info_value(
                    &path.display().to_string(),
                    loc,
                    &reason,
                ));
            } else {
                crate::out::err(format_args!("{}", crate::sanitized(&msg)));
            }
        }
    }
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
            if crate::out::json() {
                crate::out::emit(&crate::out::info_value(
                    &path.display().to_string(),
                    Some((loc.line, loc.column)),
                    &format!("fix refused: {e}"),
                ));
            } else {
                crate::out::err(format_args!("{msg}"));
            }
        }
    }
}

/// A minimal unified diff (3 lines of context) for `--dry-run`. Line-based
/// LCS — fix targets are configuration files, small by nature; a
/// pathological pair falls back to one whole-file hunk rather than
/// quadratic work.
pub(crate) fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let new_lines: Vec<&str> = new.split_inclusive('\n').collect();

    // The labels are `a/<path>` and `b/<path>` as git spells them: an
    // absolute path drops its leading separator rather than printing
    // `a//var/…`.
    let spelled = crate::sanitized(&path.display().to_string());
    let label = spelled.trim_start_matches(['/', '\\']);
    let mut out = format!("--- a/{label}\n+++ b/{label}\n");
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
            // Content lines are REPO BYTES headed for a terminal: the
            // very files this tool triages carry raw controls (that is
            // why they have fixes), so each line body is sanitized like
            // every other surface that echoes source. The line
            // TERMINATOR (LF or CRLF — legal transport) stays literal:
            // sanitizing it would escape every line ending; a bare CR
            // inside the body is content and renders escaped.
            let (body, term) = if let Some(b) = line.strip_suffix("\r\n") {
                (b, "\r\n")
            } else if let Some(b) = line.strip_suffix('\n') {
                (b, "\n")
            } else {
                (line, "")
            };
            out.push_str(&crate::sanitized(body));
            if term.is_empty() {
                out.push_str("\n\\ No newline at end of file\n");
            } else {
                out.push_str(term);
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

/// `(old_index, new_index, kind)` edit script. Indices are the positions
/// each op consumes (for `Insert`, `old_index` is where it lands; for
/// `Delete`, `new_index` likewise) — exactly what hunk headers need.
///
/// The common prefix and suffix are matched off first, and only what is
/// left between them reaches the quadratic LCS. Every real diff does this,
/// and here it is what keeps `--dry-run` readable: a one-line change in a
/// 3,300-line file used to overrun [`MAX_CELLS`] and render as a
/// whole-file replacement — 6,600 lines of "diff" for one blank line.
fn diff_ops(old: &[&str], new: &[&str]) -> Vec<(usize, usize, OpKind)> {
    let head = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut ops: Vec<(usize, usize, OpKind)> = (0..head).map(|i| (i, i, OpKind::Equal)).collect();
    ops.extend(
        diff_ops_lcs(&old[head..old.len() - tail], &new[head..new.len() - tail])
            .into_iter()
            .map(|(o, n, k)| (o + head, n + head, k)),
    );
    ops.extend((0..tail).map(|i| (old.len() - tail + i, new.len() - tail + i, OpKind::Equal)));
    ops
}

/// The edit script for the two SIDES that differ, by LCS. A pathological
/// pair falls back to one whole-block replacement rather than quadratic
/// work ([`MAX_CELLS`]).
fn diff_ops_lcs(old: &[&str], new: &[&str]) -> Vec<(usize, usize, OpKind)> {
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
        a_s(parse_clean, msgs, 0)
    }

    /// [`a`] with a reported suppressed count — the Σ-deficit budget.
    fn a_s(parse_clean: bool, msgs: &[&str], suppressed: usize) -> Analysis {
        Analysis {
            parse_clean,
            diags: msgs.iter().map(|m| d(m)).collect(),
            suppressed,
            own: "main.nml".to_string(),
        }
    }

    fn key(msg: &str) -> FindingKey {
        (
            Some(codes::SEALED_FIELD_VIOLATION),
            Some((0, 1)),
            msg.to_string(),
            Some("main.nml".to_string()),
        )
    }

    /// E35 (arch finding 2): `analyze` validates against the validator
    /// the per-file context hands it and cannot rebuild the binding's.
    /// Structural, timing-free: the binding's own validator (`core`:
    /// `thing.v string`) finds nothing in `plain.flow.nml`; a DIFFERENT
    /// validator handed in through the context is the one the analysis
    /// uses — it draws the missing-field finding for a field the binding
    /// never declared. Had `analyze` re-resolved and rebuilt from the
    /// binding, it could not have seen `w`.
    #[test]
    fn analyze_validates_against_the_context_validator_not_a_rebuilt_one() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace");
        let target = root.join("tenants/cu/plain.flow.nml");
        let ws = crate::workspace::Workspace::open(Some(&root), &[target.display().to_string()])
            .expect("opens");
        let resolved = ws.resolve(&target).expect("resolves");
        assert!(matches!(
            resolved.governing,
            nml_validate::workspace::Governing::Bound { .. }
        ));
        let source = std::fs::read_to_string(&target).unwrap();
        let bound = crate::workspace::judge(&resolved, None, &target)
            .ok()
            .flatten()
            .expect("the binding's validator");
        let ctx = FixContext {
            path: &target,
            schema_dir: None,
            resolved: &resolved,
            validator: Some(&bound),
            vocabulary: None,
        };
        assert!(
            analyze(&ctx, &source).diags.is_empty(),
            "the binding's validator is satisfied"
        );
        let extracted =
            nml_core::cst::extract_schema("model thing:\n    v string\n    w string\n").0;
        let other = SchemaValidator::new(extracted.models, extracted.enums, extracted.oneofs);
        let ctx = FixContext {
            validator: Some(&other),
            ..ctx
        };
        let diags = analyze(&ctx, &source).diags;
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("missing required field 'w'")),
            "the handed-in validator judged the text: {diags:?}"
        );
    }

    /// A base defect cloned into an overlay's resolved body (`w = 1`
    /// under `w string`, inherited by `over uses base`) is ONE finding
    /// in `fix`'s analysis, as `nml check` reports one: the validator
    /// runs through the kernel's `Deduped` sink seeded by the composed
    /// findings (`ComposedFile::dedup_seed`). A set of this verb's own,
    /// or no set at all, counted the clone twice — and the round gate's
    /// multiset then judged a phantom.
    #[test]
    fn analyze_deduplicates_a_base_defect_cloned_into_an_overlay() {
        let dir = crate::scratch::Scratch::new("fix-dedup");
        let target = dir.join("a.flow.nml");
        let text = "model thing:\n    v string\n    w string\n\nthing base:\n    v = \"x\"\n    \
                    w = 1\n\nthing over uses base:\n    v = \"y\"\n";
        std::fs::write(&target, text).unwrap();
        let ws = crate::workspace::Workspace::open(Some(&dir), &[target.display().to_string()])
            .expect("opens");
        let resolved = ws.resolve(&target).expect("resolves");
        let ctx = FixContext {
            path: &target,
            schema_dir: None,
            resolved: &resolved,
            validator: None,
            vocabulary: None,
        };
        let diags = analyze(&ctx, text).diags;
        let mismatches = diags
            .iter()
            .filter(|d| d.message.starts_with("type mismatch for 'w'"))
            .count();
        assert_eq!(mismatches, 1, "one home per finding: {diags:?}");
    }

    #[test]
    fn round_gate_keys_by_source() {
        // Same text, two sources: the other file's finding is not this
        // key's to spend. Before: `k` in main AND `k` in other; the round
        // applies main's; after: main's SURVIVES and other's vanished. A
        // source-blind key would net the counts (2 → 1, applied 1:
        // deficit 0) and accept a genuinely failed fix.
        let other = d("k").with_source("other.nml".to_string());
        let before = Analysis {
            parse_clean: true,
            diags: vec![d("k"), other],
            suppressed: 0,
            own: "main.nml".to_string(),
        };
        let after = a(true, &["k"]);
        assert!(!round_improved(&before, &after, &[&key("k")]));
        // And the honest decrement on the SAME key still lands: an
        // unstamped finding reads as `main.nml`, the analysis's own.
        let after = Analysis {
            parse_clean: true,
            diags: vec![d("k").with_source("other.nml".to_string())],
            suppressed: 0,
            own: "main.nml".to_string(),
        };
        assert!(round_improved(&before, &after, &[&key("k")]));
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
    fn gate_accepts_flood_slack_within_the_suppressed_budget() {
        // The Σ-deficit clause (D-A): a capped flood's hidden instances
        // may re-surface on the applied key, up to EXACTLY the reported
        // suppressed count. Miniature of the 129-instance flood: 2
        // visible + 1 suppressed; the round applies both visible; one
        // hidden instance surfaces — deficit 1 ≤ budget 1, accepted.
        let (before, after) = (a_s(false, &["k", "k"], 1), a_s(false, &["k"], 0));
        assert!(round_improved(&before, &after, &[&key("k"), &key("k")]));
    }

    #[test]
    fn gate_rejects_flood_slack_beyond_the_suppressed_budget() {
        // Two survivors against a budget of one: the second survivor is
        // NOT explicable by the cap — a genuinely moved finding — and
        // the round reverts.
        let (before, after) = (a_s(false, &["k", "k"], 1), a_s(false, &["k", "k"], 0));
        assert!(!round_improved(&before, &after, &[&key("k"), &key("k")]));
    }

    #[test]
    fn gate_never_lets_a_failing_key_borrow_anothers_slack() {
        // Key m over-delivers (applied 1, count fell 2) while key k
        // fails outright (applied 1, count unchanged). The per-key
        // floor keeps m's surplus from crediting k: the deficit is
        // still 1, and with nothing suppressed the round fails —
        // an unfloored sum would have netted to zero and accepted a
        // genuinely failed fix.
        let (before, after) = (a(true, &["k", "m", "m"]), a(true, &["k"]));
        assert!(!round_improved(&before, &after, &[&key("k"), &key("m")]));
        // Even a real suppressed budget is for cap-hidden findings, not
        // for outright failures beyond it: deficit 3 > budget 1.
        let (before, after) = (a_s(false, &["k", "k"], 1), a_s(false, &["k", "k", "k"], 0));
        assert!(!round_improved(&before, &after, &[&key("k"), &key("k")]));
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

    /// The round budget's floor and ceiling are
    /// the published bounds, and the diff matrix falls back to one
    /// whole-file hunk past [`MAX_CELLS`] instead of quadratic work.
    #[test]
    fn round_budget_and_diff_matrix_are_bounded_by_the_published_constants() {
        assert_eq!(round_budget(0), MIN_ROUNDS);
        assert_eq!(round_budget(3), 3 + MIN_ROUNDS);
        assert_eq!(round_budget(MAX_ROUNDS * 4), MAX_ROUNDS);
        let n = MAX_CELLS.isqrt() + 1;
        // A long file with ONE changed line diffs exactly, however long it
        // is: the prefix and suffix match off and the LCS never runs. This
        // is what the reader of `--dry-run` sees.
        let mut old: Vec<&str> = vec!["same\n"; n];
        let mut new = old.clone();
        old[n / 2] = "was\n";
        new[n / 2] = "now\n";
        let ops = diff_ops(&old, &new);
        assert_eq!(
            ops.iter()
                .filter(|(_, _, k)| !matches!(k, OpKind::Equal))
                .count(),
            2,
            "one changed line is one delete and one insert, whatever the file's length"
        );
        // Past the cell bound, what is left BETWEEN the matched ends still
        // falls back to one whole-block replacement rather than quadratic
        // work.
        let old: Vec<&str> = (0..n).map(|_| "a\n").collect();
        let new: Vec<&str> = (0..n).map(|_| "b\n").collect();
        let ops = diff_ops(&old, &new);
        assert!(
            ops.iter().all(|(_, _, k)| !matches!(k, OpKind::Equal)),
            "past the cell bound the diff is one whole-block replacement"
        );
        assert_eq!(ops.len(), 2 * n);
        let small: Vec<&str> = vec!["same\n"; 4];
        assert!(
            diff_ops(&small, &small)
                .iter()
                .all(|(_, _, k)| matches!(k, OpKind::Equal))
        );
    }

    /// A fix round's `analyze` parses the file ONCE (the
    /// extraction feeds the loader, as in `check`); read at the parse
    /// counter, structurally.
    #[test]
    fn analyze_parses_the_file_exactly_once() {
        use nml_core::cst::parses_on_this_thread;
        let dir = crate::scratch::Scratch::new("fix-one-parse");
        let target = dir.join("a.flow.nml");
        let source = "model thing:\n    v string\n\nthing T:\n    v = \"x\"\n";
        std::fs::write(&target, source).unwrap();
        let ws = crate::workspace::Workspace::open(Some(&dir), &[target.display().to_string()])
            .expect("opens");
        let resolved = ws.resolve(&target).expect("resolves");
        let ctx = FixContext {
            path: &target,
            schema_dir: None,
            resolved: &resolved,
            validator: None,
            vocabulary: None,
        };
        let before = parses_on_this_thread();
        let analysis = analyze(&ctx, source);
        assert_eq!(parses_on_this_thread() - before, 1, "one parse per round");
        assert!(
            analysis.parse_clean && analysis.diags.is_empty(),
            "{:?}",
            analysis.diags
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
