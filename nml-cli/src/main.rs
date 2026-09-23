// No `unsafe` in this crate (RFC 0019 item 0, E35): enforced at the root.
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use nml_core::diagnostic::{Code, Diagnostic, Severity};
use nml_core::layers::LayersWire;
use nml_core::span::Span;
use nml_validate::schema::SchemaValidator;
use nml_validate::workspace::{Governing, OpenError, ReadError, SymlinkVerdict, read_beneath};

mod fix;
mod invocation;
mod limits;
mod out;
#[cfg(test)]
mod scratch;
mod workspace;

use invocation::{Arity, Edit, Invocation, Parsed, Spec};

/// Parse a file via the CST, reporting **every** syntactic and semantic error
/// at once (not just the first — exceeding the legacy one-at-a-time UX). Returns
/// the AST when the input is fully valid.
fn parse_or_report_all(
    ws: Option<&workspace::Workspace>,
    path: &Path,
    own: &str,
    source: &str,
) -> Result<nml_core::ast::File, String> {
    let (file, errors) = nml_core::cst::parse_to_ast_all(source);
    report_parse_findings(ws, path, own, source, file, errors)
}

/// [`parse_or_report_all`]'s reporting half over an already-parsed file:
/// the AST when the findings are empty, else every finding printed and
/// the same `N parse error(s)` failure. The workspace verbs parse ONCE
/// (`parse_and_extract_split`) and report through here, so the schema
/// loader below them never re-parses the target.
fn report_parse_findings(
    ws: Option<&workspace::Workspace>,
    path: &Path,
    own: &str,
    source: &str,
    file: nml_core::ast::File,
    errors: Vec<Diagnostic>,
) -> Result<nml_core::ast::File, String> {
    if errors.is_empty() {
        return Ok(file);
    }
    let source_map = nml_core::span::SourceMap::new(source);
    for e in &errors {
        report(ws, path, own, source, &source_map, e);
    }
    // The suppressed-count row is an info diagnostic riding the same
    // vec — counting it printed "129 parse error(s)" for a 128-error
    // flood.
    let error_count = errors
        .iter()
        .filter(|e| matches!(e.severity, Severity::Error))
        .count();
    Err(format!("{error_count} parse error(s)"))
}

/// Which code the explain hint names (E35, arch finding 2/5): the first
/// coded ERROR of the run, falling back to the first coded warning —
/// never a warning that happened to print before the error (an
/// inert-input NML2080 note printed above an NML2064 error must not
/// take the hint).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Hint {
    error: Option<Code>,
    warning: Option<Code>,
}

impl Hint {
    pub(crate) fn note(&mut self, diag: &Diagnostic) {
        match diag.severity {
            Severity::Error => self.error = self.error.or(diag.code),
            Severity::Warning => self.warning = self.warning.or(diag.code),
            _ => {}
        }
    }

    pub(crate) fn code(self) -> Option<Code> {
        self.error.or(self.warning)
    }
}

/// ONE explain-hint rule per RUN: every diagnostic the two
/// reporters print is noted here, and the hint — rustc's "for more
/// information, run: nml explain …" — prints ONCE at the end of the run,
/// naming the run's first coded error, else its first coded warning.
/// Pre-fold each verb kept its own accumulator: `validate` dropped the
/// universe's warning before the symbols pass (a warning-only run
/// printed no hint), and a multi-file `fix` printed a hint per rejected
/// file.
static RUN_HINT: Mutex<Hint> = Mutex::new(Hint {
    error: None,
    warning: None,
});
static HINT_FLUSHED: AtomicBool = AtomicBool::new(false);

fn note_hint(diag: &Diagnostic) {
    if let Ok(mut hint) = RUN_HINT.lock() {
        hint.note(diag);
    }
}

/// `--strict` beside a file a binding governs: the binding's own
/// `strict` is the file's strictness in every front end (the editor
/// has no flag to tighten it with, and E39 wants one verdict), so the
/// flag does not apply — said ONCE per run, for the first file whose
/// binding is lenient, naming the binding to set `strict = true` on
/// (nothing for a binding that is strict already: the flag changes
/// nothing there; nothing under `--json` or `--quiet`, like the root
/// note). It used to apply: `nml check --strict` made the CI's verdict
/// on a bound file stricter than the editor's on the same file.
fn strict_does_not_apply(resolved: &nml_validate::workspace::Resolved<'_>, file_arg: &str) {
    static SAID: AtomicBool = AtomicBool::new(false);
    let nml_validate::workspace::Governing::Bound { claimant, .. } = &resolved.governing else {
        return;
    };
    if claimant.binding.strict || out::json() || out::quiet() || SAID.swap(true, Ordering::Relaxed)
    {
        return;
    }
    out::err(format_args!(
        "{} --strict does not apply to {}: a manifest-governed file validates under its \
         binding's own strictness, as the editor does — binding '{}' of {} declares no `strict = \
         true`; set it on the binding to enforce it everywhere",
        out::paint(out::Level::Note, "note:"),
        sanitized(file_arg),
        sanitized(&claimant.binding.name),
        sanitized(claimant.claim.manifest_label())
    ));
}

/// Whether the run has reported a coded finding — the rows a closing
/// sentence may point back at with "the error(s) above". A door refusal
/// that printed no row (a target naming no `.nml` file) has none, and a
/// verdict that blamed findings the reader could not see was simply
/// untrue.
pub(crate) fn reported_a_coded_finding() -> bool {
    RUN_HINT.lock().ok().and_then(|h| h.code()).is_some()
}

/// Print the run's explain hint (once; nothing under `--json`, where
/// every row carries its code, nothing under `--quiet`, and nothing
/// when no coded finding printed).
pub(crate) fn flush_explain_hint() {
    if HINT_FLUSHED.swap(true, Ordering::Relaxed) || out::json() || out::quiet() {
        return;
    }
    let code = RUN_HINT.lock().ok().and_then(|h| h.code());
    if let Some(code) = code {
        out::err(format_args!(
            "for more information, run: nml explain {code}"
        ));
    }
}

/// Universe notes print ONCE per run: a directory `fix` walk
/// or a multi-target `check` resolves many files under one inert input,
/// and each resolution carries the same NML2080 note; the note is
/// deduplicated on `(source, code, message)` across the run.
/// A printed-note identity: (source, code, message) — the dedup key.
type NoteKey = (String, Option<Code>, String);
static PRINTED_NOTES: Mutex<Option<HashSet<NoteKey>>> = Mutex::new(None);

fn first_print(source: &str, diag: &Diagnostic) -> bool {
    let key = (source.to_string(), diag.code, diag.message.clone());
    let Ok(mut printed) = PRINTED_NOTES.lock() else {
        return true;
    };
    printed.get_or_insert_with(HashSet::new).insert(key)
}

/// A related note located for printing — the FACTS both renderers
/// read: its source (a key, or the file's own name), its `(line, col)`
/// when that source could be mapped, its message, and the note itself
/// (for the byte span when it could not). The human `note:` line and
/// the `--json` `related[]` entry are rendered from ONE of these by
/// [`report`] and [`report_locationless`] alike, so both locate a note
/// in its own file.
struct LocatedNote<'d> {
    source: String,
    loc: Option<(usize, usize)>,
    message: String,
    rel: &'d nml_core::diagnostic::Related,
}

impl LocatedNote<'_> {
    /// The `--json` `related[]` entry.
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "source": self.source,
            "line": self.loc.map(|l| l.0),
            "col": self.loc.map(|l| l.1),
            "message": self.message,
        })
    }

    /// The human line — `<file>:<line>:<col>: note: <message>`, located
    /// in the note's own file; `<file>: note: <message> (bytes
    /// <start>..<end>)` when that file could not be read — never the
    /// right file with a wrong range. `spelled` is the source as this
    /// line spells it (the path as typed for a same-file note beside a
    /// located finding; the key otherwise). Message and path both
    /// sanitized.
    fn line(&self, spelled: &str) -> String {
        let note = out::paint(out::Level::Note, "note:");
        match self.loc {
            Some((line, col)) => format!(
                "{}:{line}:{col}: {note} {}",
                sanitized(spelled),
                sanitized(&self.message)
            ),
            None => format!(
                "{}: {note} {} (bytes {}..{})",
                sanitized(spelled),
                sanitized(&self.message),
                self.rel.span.start,
                self.rel.span.end
            ),
        }
    }
}

/// A diagnostic with no span in the checked file — a universe-level
/// note (a truncated walk, an inert input, a manifest that failed to
/// load) or a kernel rejection of the path itself (NML2083): printed as
/// `<source>: <severity>[<code>]: <message>` against ITS OWN source (a
/// workspace-relative key, sanitized like every path), or as
/// `<source>:<line>:<col>: …` when the note is spanned in a live
/// manifest's own text (NML2092 at a glob), located through the
/// universe (`Workspace::manifest_location` — no second read). Printed
/// once per run per `(source, code, message)`. The universe also locates
/// a foreign note (`Related.source`, read through the root like every
/// note): a kernel finding that names another file's finding — NML2091's
/// first failing source line — prints it as a `note:` line beneath the
/// row, and rides the row's `related[]` under `--json`, exactly as a
/// located finding's notes do — a note in the finding's OWN file too
/// (no text of it is in hand here): a live manifest's through the
/// universe's one derivation over the text it judged, never a second
/// read; a file the root cannot read leaves the note in the byte-span
/// form.
pub(crate) fn report_locationless(
    ws: &workspace::Workspace,
    fallback: &str,
    diag: &Diagnostic,
) -> Option<Code> {
    let source = diag.source.as_deref().unwrap_or(fallback);
    if !first_print(source, diag) {
        return diag.code;
    }
    note_hint(diag);
    if !out::admit(diag) {
        return diag.code;
    }
    if out::json() {
        out::emit(&locationless_value(ws, source, diag));
        return diag.code;
    }
    let at = ws.manifest_location(diag);
    let empty = nml_core::span::SourceMap::new("");
    let mut foreign: std::collections::HashMap<&str, Option<Foreign>> =
        std::collections::HashMap::new();
    // No text of the finding's own file is in hand here (it was never
    // read as a target), so EVERY note — one in the finding's own file
    // included — is located as a foreign note: a live manifest's by the
    // universe's one derivation over the text it judged (the first
    // `files` of a manifest's repeated entry, NML2093, prints
    // `key:line:col:` beneath the row), any other file read through
    // the root.
    let notes: Vec<LocatedNote<'_>> = diag
        .related
        .iter()
        .map(|rel| note_loc(Some(ws), &empty, "", &mut foreign, diag, rel))
        .collect();
    out::err(format_args!(
        "{}",
        universe_note_line(source, at, diag, true)
    ));
    for note in &notes {
        out::err(format_args!("{}", note.line(&note.source)));
    }
    // Every edit is foreign too — one in the finding's own file
    // included — resolved through the universe's kept text.
    report_insertions(
        Some(ws),
        Path::new(source),
        "",
        &empty,
        "",
        &mut foreign,
        diag,
    );
    diag.code
}

/// The `--json` row of a finding no target's text is in hand for — a
/// universe note, a kernel row on a key, `binding`'s `notes[]`: located
/// through the universe (`Workspace::manifest_location`), its notes and
/// its cause located as foreign places, its edits resolved as foreign
/// edits — one in the finding's own file included, since no text of
/// it is in hand here: a failed manifest's did-you-mean resolves
/// against the text the universe kept. ONE builder,
/// so `binding`'s `notes[]` carry what `check`'s rows carry.
fn locationless_value(
    ws: &workspace::Workspace,
    source: &str,
    diag: &Diagnostic,
) -> serde_json::Value {
    let empty = nml_core::span::SourceMap::new("");
    let mut foreign: std::collections::HashMap<&str, Option<Foreign>> =
        std::collections::HashMap::new();
    let related: Vec<serde_json::Value> = diag
        .related
        .iter()
        .map(|rel| note_loc(Some(ws), &empty, "", &mut foreign, diag, rel).json())
        .collect();
    let suggestions: Vec<serde_json::Value> = diag
        .suggestions
        .iter()
        .filter_map(|s| resolve_suggestion(Some(ws), "", &empty, "", &mut foreign, diag, s))
        .map(|r| r.json())
        .collect();
    let cause = cause_value(Some(ws), &empty, "", &mut foreign, diag);
    out::diag_value(
        source,
        ws.manifest_location(diag),
        diag,
        related,
        suggestions,
        cause,
    )
}

/// One universe note as a human reads it: `<source>: <severity>[<code>]:
/// <message>`, or `<source>:<line>:<col>: …` when the note is located in
/// a manifest's own text — the shape `binding`'s `notes` lines and the
/// locationless reporter share, never the raw byte-span suffix `Display`
/// adds for span-less contexts. `painted` for the stderr reporter;
/// plain for the block on stdout.
fn universe_note_line(
    source: &str,
    loc: Option<(usize, usize)>,
    diag: &Diagnostic,
    painted: bool,
) -> String {
    let at = loc.map(|(l, c)| format!(":{l}:{c}")).unwrap_or_default();
    format!(
        "{}{at}: {}: {}",
        sanitized(source),
        out::severity_prefix(diag, painted),
        diag.rendered()
    )
}

/// The universe's word on ONE file, printed before the file is read:
/// the inert inputs on its chain (NML2080) and the kernel's own
/// findings on the key (NML2083, a unit's NML2089). Returns the error
/// count: ANY error here means nothing is validated — a path a closed
/// binding rejects fails the verb with exit 1 and the file is never
/// opened (E26, E28). Shared by every verb, `fix` included (E35). The
/// universe's word on EVERY file is [`report_universe_notes`]'s, stated
/// once per run before any target — never here, so no target can
/// repeat it.
pub(crate) fn report_universe(
    fallback: &str,
    ws: &workspace::Workspace,
    resolved: &nml_validate::workspace::Resolved<'_>,
) -> usize {
    report_rows(
        ws,
        fallback,
        ws.discovery()
            .inert_notes_for(&resolved.key)
            .iter()
            .chain(&resolved.findings),
    )
}

/// The universe's word on EVERY file, stated ONCE per run where the
/// universe is stated — before any target, on stderr, as `diagnostic`
/// rows before any per-target row under `--json`: its errors (a
/// truncated walk, a live manifest or project config that failed to
/// load) or, when it stands, its unit-layout notes (NML2092). The ONE
/// printer every workspace verb states the universe through —
/// `binding` included, which used to repeat these rows in every
/// target's block and tally them per block. Returns the error count.
pub(crate) fn report_universe_notes(ws: &workspace::Workspace, fallback: &str) -> usize {
    report_rows(ws, fallback, &ws.universe_notes())
}

/// Print `rows` through the locationless reporter and count the errors
/// among them — the one loop the universe's notes, the gate's rows and
/// the refusal's errors all go through.
fn report_rows<'d>(
    ws: &workspace::Workspace,
    fallback: &str,
    rows: impl IntoIterator<Item = &'d Diagnostic>,
) -> usize {
    let mut errors = 0;
    for diag in rows {
        report_locationless(ws, fallback, diag);
        if matches!(diag.severity, Severity::Error) {
            errors += 1;
        }
    }
    errors
}

/// `--max-findings <n>`: the run's reporting budget.
/// A verb that does not offer the flag keeps the default.
fn apply_budget(inv: &Invocation) {
    if let Some(n) = inv.max_findings {
        out::set_budget(n);
    }
}

/// The verb this run is executing, recorded before it parses anything
/// so every exit path (a usage error inside the pipeline included) can
/// name it on the run's closing row.
static RUN_VERB: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The run's ONE ending: the explain hint, the
/// budget's withheld-count trailer, then the closing `summary` row —
/// emitted on EVERY path, so a `--json` consumer always reads a terminal
/// row carrying the run's `exit` instead of re-encoding each verb's
/// 0/1/2 mapping. Nothing else in this crate may exit — except a
/// `--help` page, which is output rather than a run and leaves through
/// `out::help_page` with no closing row, and the two pre-verb
/// exits in `main` (no arguments, an unknown verb), before any `--json`
/// is parsed.
fn finish(exit: i32) -> ! {
    out::report_withheld();
    flush_explain_hint();
    let verb = RUN_VERB.get().map_or("nml", String::as_str);
    out::emit_summary(verb, exit);
    out::exit(exit)
}

/// A usage/configuration error: the invocation contradicts the universe
/// it runs in (D-0d-1's `--schema` conflict). Exit code 2, distinct from
/// a failing check (1), so CI can tell "the file is wrong" from "the
/// command is wrong".
pub(crate) fn exit_usage(message: &str) -> ! {
    out::report_withheld();
    flush_explain_hint();
    if out::json() {
        out::error_line(message, 2, "usage");
        finish(2)
    }
    // Line by line: the sanitizer escapes control characters (a
    // newline included), and this surface is multi-line by design.
    for (i, line) in message.lines().enumerate() {
        if i == 0 {
            out::err(format_args!(
                "{} {}",
                out::paint(out::Level::Error, "error:"),
                sanitized(line)
            ));
        } else {
            out::err(format_args!("{}", sanitized(line)));
        }
    }
    finish(2)
}

/// The one diagnostic printer: `path:line:col: <Display>` — the path AS
/// TYPED, then severity, the stable `[NML0000]` code when assigned, and
/// the did-you-mean hint derived from the structured suggestion (RFC
/// 0008). `own` is the file's NAME on the wire (the `--json` row's
/// `source`, and what a same-file `Related.source` equals): its workspace
/// key for a workspace file (step 0f), its basename for a `--schema`
/// source, the path as typed for the workspace-free verbs. `ws` is the
/// universe a foreign note's file is read under (A3); `parse` and `fmt`
/// pass `None` and a foreign note renders without a range. `source` is
/// the file's text: a finding's edits resolve against the file they
/// edit — this one, or a foreign one read through the universe.
fn report(
    ws: Option<&workspace::Workspace>,
    path: &Path,
    own: &str,
    source: &str,
    source_map: &nml_core::span::SourceMap,
    diag: &Diagnostic,
) -> Option<Code> {
    note_hint(diag);
    // The per-run, per-code budget: the finding is
    // TALLIED exactly here whether or not it is printed, so a capped run
    // still reports its counts and its exit code truthfully.
    if !out::admit(diag) {
        return diag.code;
    }
    let (line, column) = match diag.span {
        Some(span) => {
            let loc = source_map.location(span.start);
            (loc.line, loc.column)
        }
        None => (0, 0),
    };
    // Secondary locations (RFC 0009) — rustc's `note:` shape, each
    // located in ITS OWN file (`Related.source`, RFC 0019 plan item 2):
    // the checked file's map for same-file notes; a foreign path is read
    // THROUGH THE UNIVERSE on first use, cached across this diagnostic's
    // notes.
    let mut foreign: std::collections::HashMap<&str, Option<Foreign>> =
        std::collections::HashMap::new();
    if out::json() {
        let loc = diag.span.map(|_| (line, column));
        let related: Vec<serde_json::Value> = diag
            .related
            .iter()
            .map(|rel| note_loc(ws, source_map, own, &mut foreign, diag, rel).json())
            .collect();
        let suggestions: Vec<serde_json::Value> = diag
            .suggestions
            .iter()
            .filter_map(|s| resolve_suggestion(ws, source, source_map, own, &mut foreign, diag, s))
            .map(|r| r.json())
            .collect();
        let cause = cause_value(ws, source_map, own, &mut foreign, diag);
        out::emit(&out::diag_value(
            own,
            loc,
            diag,
            related,
            suggestions,
            cause,
        ));
        return diag.code;
    }
    // `line:col` already locates the finding — the raw byte-span suffix that
    // `Display` adds for span-less contexts would be noise here.
    // `path` can be a WALKED schema file (`--schema <dir>` attribution),
    // not only an argv path — a hostile filename must not smuggle
    // terminal escapes (the message itself renders through the
    // sanitizing `Rendered`).
    out::err(format_args!(
        "{}:{}:{}: {}: {}",
        sanitized(own),
        line,
        column,
        out::severity_prefix(diag, true),
        diag.rendered()
    ));
    for rel in &diag.related {
        out::err(format_args!(
            "{}",
            note_line(ws, path, source_map, own, &mut foreign, diag, rel)
        ));
    }
    report_insertions(ws, path, source, source_map, own, &mut foreign, diag);
    diag.code
}

/// A foreign file's text for a finding's edits (they resolve against
/// it) and its line index — a live manifest's as the universe holds
/// it, any other file's read through the universe.
struct Foreign {
    text: String,
    map: nml_core::span::SourceMap,
}

/// A foreign source's text: a live manifest's as the universe holds it
/// — the bytes its verdict read, loaded or kept after a failed load —
/// else read through the universe (`Workspace::read_source`: resolved
/// under the invocation's root and read through the verdict, never by
/// cwd-relative path); `None` when it cannot be.
fn read_foreign(ws: Option<&workspace::Workspace>, src: &str) -> Option<Foreign> {
    let ws = ws?;
    let text = match ws.discovery().manifest_text(src) {
        Some(text) => text.to_string(),
        None => ws.read_source(src).ok()?,
    };
    Some(Foreign {
        map: nml_core::span::SourceMap::new(&text),
        text,
    })
}

/// One byte-exact edit of a resolved suggestion: `[start, end)` as
/// 1-based `(line, column)` pairs in its file, and the replacement.
struct ResolvedEdit {
    start: (usize, usize),
    end: (usize, usize),
    replacement: String,
}

/// One machine-applicable edit set of a finding, RESOLVED against the
/// file it edits as this run read it (`resolve_suggestions` — the one
/// resolver every applier shares, so the bytes here are the bytes `nml
/// fix` and the editor would splice): the `--json` `suggestions[]` entry
/// and the human `help:` block draw on these same facts.
struct ResolvedSuggestion<'d> {
    suggestion: &'d nml_core::diagnostic::Suggestion,
    /// The edited file's name on the wire (its key; the checked file's
    /// own when the edit is in it).
    source: String,
    edits: Vec<ResolvedEdit>,
}

impl ResolvedSuggestion<'_> {
    /// The `--json` `suggestions[]` entry: `{kind, source, edits[{line,
    /// col, endLine, endCol, lines}]}` — `lines` joined with a newline is
    /// the exact replacement, and no element ever hosts a control
    /// character (the resolver's guard; an insertion's newlines are its
    /// line structure, split here), so a consumer prints or applies a
    /// line as a line.
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.suggestion.kind.wire_name(),
            "source": self.source,
            "edits": self
                .edits
                .iter()
                .map(|e| serde_json::json!({
                    "line": e.start.0,
                    "col": e.start.1,
                    "endLine": e.end.0,
                    "endCol": e.end.1,
                    "lines": e.replacement.split('\n').collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Resolve one suggestion against the file it edits — the checked file's
/// own text, or a foreign file's read through the universe and cached
/// across this finding's notes and edits (`suggestion_source`: the same
/// inheritance a note has). `None` when the kernel refused it (a stale
/// anchor, a snippet the gate rejects, a file that cannot be read now):
/// an edit the run cannot vouch for is on no surface.
fn resolve_suggestion<'d>(
    ws: Option<&workspace::Workspace>,
    source: &str,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    diag: &'d Diagnostic,
    suggestion: &'d nml_core::diagnostic::Suggestion,
) -> Option<ResolvedSuggestion<'d>> {
    let foreign_src = diag.suggestion_source(suggestion).filter(|s| *s != own);
    let (text, map, name) = match foreign_src {
        None => (source, source_map, own),
        Some(src) => {
            let file = foreign
                .entry(src)
                .or_insert_with(|| read_foreign(ws, src))
                .as_ref()?;
            (file.text.as_str(), &file.map, src)
        }
    };
    let resolved = nml_core::cst::edit::resolve_suggestions(text, std::slice::from_ref(suggestion));
    resolved.outcomes.first()?.as_ref().ok()?;
    if resolved.edits.is_empty() {
        return None;
    }
    let edits = resolved
        .edits
        .into_iter()
        .map(|e| {
            let start = map.location(e.span.start);
            let end = map.location(e.span.end);
            ResolvedEdit {
                start: (start.line, start.column),
                end: (end.line, end.column),
                replacement: e.replacement,
            }
        })
        .collect();
    Some(ResolvedSuggestion {
        suggestion,
        source: name.to_string(),
        edits,
    })
}

/// A finding's remedy blocks — every `Insert` suggestion, rustc's `help:`
/// with a suggested replacement — beneath the row and its notes: `help:`
/// names the file and the line the block goes after, then the block AS
/// RESOLVED against that file (its own indentation, whatever the width),
/// one line per row, so a terminal copy pastes as printed. Every line is
/// sanitized on its own and opens with the body's indentation, so content
/// a producer failed to escape can at most break INTO the block, never
/// open a column-0 row (a forged `<file>: ok (…)` or `<file>:1:1:
/// error[…]`); the lines are content, so the ASCII fold spells their
/// glyphs as the `\u{…}` escapes the manifest reads back, never the tool's
/// typography (`out::err_content`). An edit the kernel could not resolve
/// (the file changed since the run read it, or cannot be read now)
/// prints the producer's zero-indent block one canonical step in
/// (`nml_core::cst::INDENT_UNIT` — the insertion rule's last arm: no file
/// to read a unit from), saying so. The other kinds render inline in the message (`Rendered`)
/// and need no block. Human output only; the `--json` row carries the
/// resolved edits themselves.
fn report_insertions<'d>(
    ws: Option<&workspace::Workspace>,
    path: &Path,
    source: &str,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    diag: &'d Diagnostic,
) {
    use nml_core::diagnostic::SuggestionKind;
    let help = || out::paint(out::Level::Help, "help:");
    // A same-file block keeps the path AS TYPED, as the notes do.
    let spelled = |name: &str| {
        if name == own {
            own.to_string()
        } else {
            name.to_string()
        }
    };
    for s in diag
        .suggestions
        .iter()
        .filter(|s| s.kind == SuggestionKind::Insert)
    {
        match resolve_suggestion(ws, source, source_map, own, foreign, diag, s) {
            Some(r) => {
                for e in &r.edits {
                    // The edit starts at a line start (after the line
                    // before it), or at the end of a last line it
                    // terminates itself: either way the block goes after
                    // the line before that point.
                    let after = if e.start.1 == 1 {
                        e.start.0.saturating_sub(1)
                    } else {
                        e.start.0
                    };
                    out::err(format_args!(
                        "{} the block to add after line {after} of {} — paste it as printed:",
                        help(),
                        sanitized(&spelled(&r.source))
                    ));
                    let block = e.replacement.strip_prefix('\n').unwrap_or(&e.replacement);
                    for line in block.lines() {
                        out::err_content(&sanitized(line));
                    }
                }
            }
            None => {
                let name = diag.suggestion_source(s).unwrap_or(own);
                out::err(format_args!(
                    "{} the block to add as the last entry of the block named at {} (bytes \
                     {}..{}), at that body's indentation — the file could not be read as the \
                     run saw it:",
                    help(),
                    sanitized(&spelled(name)),
                    s.span.start,
                    s.span.end
                ));
                for line in s.replacement.lines() {
                    out::err_content(&format!(
                        "{}{}",
                        nml_core::cst::INDENT_UNIT,
                        sanitized(line)
                    ));
                }
            }
        }
    }
}

/// Escape hostile characters for terminal output — the CLI twin of the
/// diagnostic renderer's choke point (`nml_core::diagnostic::needs_escape`):
/// note messages and note FILE PATHS print outside `Rendered`, and a
/// hostile path (a repo filename carrying an ESC byte or a bidi
/// override) must not smuggle terminal escapes.
pub(crate) fn sanitized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if nml_core::diagnostic::needs_escape(ch) {
            out.extend(ch.escape_default());
        } else {
            out.push(ch);
        }
    }
    out
}

/// One note's rendered line beside a located finding
/// ([`LocatedNote::line`]): a same-file note is spelled by the path AS
/// TYPED, a foreign one by its key.
fn note_line<'d>(
    ws: Option<&workspace::Workspace>,
    path: &Path,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    diag: &'d Diagnostic,
    rel: &'d nml_core::diagnostic::Related,
) -> String {
    let note = note_loc(ws, source_map, own, foreign, diag, rel);
    // A same-file note keeps the path AS TYPED beside the finding's own
    // prefix (the key is the wire's spelling, not the terminal's).
    if note.source == own {
        note.line(own)
    } else {
        note.line(&note.source)
    }
}

/// A place in a file, as FACTS: the file `src` names (the finding's own
/// when `None` or the same file — `own`, located through its
/// `source_map`) and its `(line, col)` when that file could be mapped.
/// A live manifest the universe holds — loaded, or kept after it failed
/// to load — is located by the universe's ONE derivation over the bytes
/// it judged (`Workspace::locate`, the row's own), never by a second
/// read that could see other bytes; a span past that text has no
/// place. Any other foreign source is read through the universe
/// (`Workspace::read_source`): resolved under the invocation's root and
/// read through the verdict, never by cwd-relative path, once per
/// finding (`foreign`). The ONE locator behind a note ([`note_loc`])
/// and a wrapping row's cause ([`cause_value`]).
fn locate_foreign<'d>(
    ws: Option<&workspace::Workspace>,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    src: Option<&'d str>,
    span: Option<Span>,
) -> (String, Option<(usize, usize)>) {
    let at = |map: &nml_core::span::SourceMap, span: Span| {
        let loc = map.location(span.start);
        (loc.line, loc.column)
    };
    let Some(src) = src.filter(|s| *s != own) else {
        return (own.to_string(), span.map(|span| at(source_map, span)));
    };
    let Some(span) = span else {
        return (src.to_string(), None);
    };
    if let Some(ws) = ws {
        if ws.discovery().manifest_text(src).is_some() {
            return (src.to_string(), ws.locate(src, span));
        }
    }
    let file = foreign.entry(src).or_insert_with(|| read_foreign(ws, src));
    (src.to_string(), file.as_ref().map(|f| at(&f.map, span)))
}

/// The located note as FACTS ([`LocatedNote`]), shared by the human line
/// and the JSON `related[]` entry so both locate a note in its own file
/// ([`locate_foreign`]); a note whose place cannot be mapped keeps the
/// byte-span form on the human line.
fn note_loc<'d>(
    ws: Option<&workspace::Workspace>,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    diag: &'d Diagnostic,
    rel: &'d nml_core::diagnostic::Related,
) -> LocatedNote<'d> {
    let (source, loc) = locate_foreign(
        ws,
        source_map,
        own,
        foreign,
        diag.related_source(rel),
        Some(rel.span),
    );
    LocatedNote {
        source,
        loc,
        message: rel.message.clone(),
        rel,
    }
}

/// The `--json` `cause` of a row that wraps another finding
/// ([`Diagnostic::cause`]): the inner finding's code and sentence, and
/// its place located in ITS OWN file by the locator a note uses
/// ([`locate_foreign`]; `line`/`col` null when the finding has no place
/// or its file cannot be read). `None` for a row that wraps nothing —
/// the key is then absent from the row.
fn cause_value<'d>(
    ws: Option<&workspace::Workspace>,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<Foreign>>,
    diag: &'d Diagnostic,
) -> Option<serde_json::Value> {
    let cause = diag.cause.as_deref()?;
    let (source, loc) = locate_foreign(
        ws,
        source_map,
        own,
        foreign,
        diag.cause_source(),
        cause.span,
    );
    Some(serde_json::json!({
        "code": cause.code.to_string(),
        "source": source,
        "line": loc.map(|l| l.0),
        "col": loc.map(|l| l.1),
        "message": cause.message,
    }))
}

/// One dispatched verb: its [`Spec`] (the parser's contract and the
/// `--help` page) and its entry point. ONE table — [`VERBS`] — that the
/// dispatcher, the unknown-command suggestion and the exits pin all
/// read, so a verb cannot be dispatched without a Spec, suggested
/// without being dispatchable, or shipped without documented exit codes
/// (a source census used to scan `main.rs` for each fn-local Spec).
struct Verb {
    spec: &'static Spec,
    run: fn(&[String]) -> Result<(), String>,
}

/// Every product verb, in the order the top-level page lists them.
/// `help` is a page, not a verb (no Spec, no exits): the dispatcher
/// answers it and the flag spellings (`--help`, `--version`) itself.
const VERBS: &[Verb] = &[
    Verb {
        spec: &PARSE,
        run: cmd_parse,
    },
    Verb {
        spec: &VALIDATE,
        run: cmd_validate,
    },
    Verb {
        spec: &FMT,
        run: cmd_fmt,
    },
    Verb {
        spec: &CHECK,
        run: cmd_check,
    },
    Verb {
        spec: &fix::SPEC,
        run: fix::cmd_fix,
    },
    Verb {
        spec: &BINDING,
        run: cmd_binding,
    },
    Verb {
        spec: &EXPLAIN,
        run: cmd_explain,
    },
    Verb {
        spec: &limits::SPEC,
        run: limits::cmd_limits,
    },
    Verb {
        spec: &VERSION,
        run: cmd_version,
    },
];

/// Every word the binary answers to, for the unknown-command
/// suggestion: the registry's verbs and the `help` page.
fn verb_names() -> Vec<&'static str> {
    VERBS.iter().map(|v| v.spec.verb).chain(["help"]).collect()
}

fn main() {
    out::flush_on_panic();
    // Arguments are taken as OS strings: one that is not UTF-8 is the
    // invocation's mistake — a usage error (exit 2, said lossily), never
    // a panic (`std::env::args` panics on it).
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let lossy: Vec<String> = raw
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    if lossy.len() < 2 {
        out::err_str(&usage_text());
        out::exit(2);
    }
    let _ = RUN_VERB.set(canonical_verb(&lossy[1]));
    let args = match utf8_args(&raw) {
        Ok(args) => args,
        Err(message) => {
            out::detect(&lossy);
            out::closing_error(&message, 2);
            finish(2)
        }
    };

    // The run's verdict, then its ONE closing row: a usage error (the
    // invocation itself — marked where it was raised) exits 2, every
    // other failure 1, so CI can tell "the file is wrong" from "the
    // command is wrong" in every verb.
    if let Err(e) = run(&args) {
        // Err strings embed walked filesystem names (an unreadable
        // subdirectory, a failing schema file) — repo content, sanitized
        // like every other surface that prints them.
        let exit = if out::is_usage() { 2 } else { 1 };
        out::report_withheld();
        flush_explain_hint();
        out::closing_error(&e, exit);
        finish(exit)
    }
    finish(0)
}

/// Every argument as UTF-8, or the usage error naming the first that is
/// not: its position and its bytes said lossily.
fn utf8_args(raw: &[std::ffi::OsString]) -> Result<Vec<String>, String> {
    raw.iter()
        .enumerate()
        .map(|(i, arg)| {
            arg.to_str().map(str::to_owned).ok_or_else(|| {
                out::usage_error(format!(
                    "argument {i} is not valid UTF-8: `{}`",
                    sanitized(&arg.to_string_lossy())
                ))
            })
        })
        .collect()
}

/// Dispatch `args[1]` as the verb — through the registry ([`VERBS`]),
/// then the words that are no verb of their own. `nml help <verb>`
/// (clig.dev: `help <subcommand>` is the same page as `<subcommand>
/// --help`) re-enters here as `nml <verb> --help`, so the two can never
/// differ; `nml help` alone is the top-level page.
fn run(args: &[String]) -> Result<(), String> {
    let word = args[1].as_str();
    if let Some(verb) = VERBS.iter().find(|v| v.spec.verb == word) {
        return (verb.run)(&args[2..]);
    }
    match word {
        "help" | "--help" | "-h" => {
            // `nml help <verb>` re-enters as `nml <verb> --help` (`nml help
            // version` is `version`'s own page that way). The help word
            // itself, and any flag, are the top-level page: `nml help
            // help` forwarded to `nml --help --help`, which forwarded to
            // itself until the stack overflowed (exit 134 on a first-time
            // operator's first guess).
            match args.get(2).map(String::as_str) {
                None | Some("help" | "--help" | "-h") => {}
                Some(flag) if flag.starts_with('-') => {}
                Some(verb) => {
                    let forwarded = vec![args[0].clone(), verb.to_string(), "--help".to_string()];
                    return run(&forwarded);
                }
            }
            // Help is output, not an error: stdout, exit 0 (clig.dev).
            out::say_str(&usage_text());
            Ok(())
        }
        "--version" | "-V" => cmd_version(&args[2..]),
        // A flag where the command goes (`nml --json check x.nml`): said
        // as a flag out of place, not as an unknown command.
        flag if flag.starts_with('-') => {
            out::err(format_args!(
                "unknown flag {}: the flags follow the command (nml <command> {} …); run `nml \
                 --help` for the command list",
                sanitized(flag),
                sanitized(flag)
            ));
            out::exit(2);
        }
        other => {
            // A near-miss gets the crate's own did-you-mean (the one
            // suggestion engine every diagnostic site uses); anything
            // else gets the command list.
            match nml_core::suggest::suggest(other, verb_names()) {
                Some(verb) => out::err(format_args!(
                    "{} unknown command: {} (did you mean `{verb}`?); run `nml --help` for \
                     the command list",
                    out::paint(out::Level::Error, "error:"),
                    sanitized(other)
                )),
                None => {
                    out::err(format_args!(
                        "{} unknown command: {}",
                        out::paint(out::Level::Error, "error:"),
                        sanitized(other)
                    ));
                    out::err_str(&usage_text());
                }
            }
            out::exit(2);
        }
    }
}

/// The verb a run's closing row names, for every spelling of it:
/// `--version`/`-V` are `version`, `--help`/`-h` are `help` (a page,
/// never a row); every other word is itself.
fn canonical_verb(typed: &str) -> String {
    match typed {
        "--version" | "-V" => "version".to_string(),
        "--help" | "-h" => "help".to_string(),
        other => other.to_string(),
    }
}

/// What `nml version` accepts, and its page ([`Spec`]).
const VERSION: Spec = Spec {
    verb: "version",
    summary: "Print the version of this nml.",
    root: false,
    schema: false,
    strict: false,
    edit: None,
    max_findings: false,
    list: false,
    targets: "",
    arity: Arity::None,
    exits: &[("0", "the version was printed"), ("2", "a usage error")],
    examples: &["nml version", "nml version --json | jq -r .version"],
};

/// `nml version`: the binary's version — a verb like every other, with
/// a `--help` page, one `version` row under `--json` (then the closing
/// row) and a surplus argument refused (it printed the version for any
/// argument and had no page; `--json` was ignored).
fn cmd_version(args: &[String]) -> Result<(), String> {
    parse_invocation(args, &VERSION)?;
    let version = env!("CARGO_PKG_VERSION");
    if out::json() {
        // ONE `version` row, then the run's closing row (which carries
        // the same string as `nmlVersion`, like every run's).
        out::emit(&serde_json::json!({"type": "version", "version": version}));
        return Ok(());
    }
    out::say(format_args!("nml {version}"));
    Ok(())
}

fn usage_text() -> String {
    "nml - NML configuration language toolkit

USAGE:
    nml <command> [options] <file>...
    nml <command> --help        a command's options (also: nml help <command>)

COMMANDS:
    parse <file>                Parse an NML file and dump the AST as JSON
                                (numbers: a JSON number for integer-form
                                values within u64, else an exact string)
    validate <path>...          Validate NML files for duplicates and
                                unresolved references (symbols only — no
                                schema validation); directories are walked
                                for .nml files
    fmt <path>...               Format NML files in place, canonical style
                                (spec/style.md); directories are walked for
                                .nml files; --check is the CI gate
    check <path>...             Parse + validate + schema check (CI-friendly);
                                directories are walked for .nml files; a
                                file a workspace manifest claims validates
                                under its binding's package and strictness
                                (then --schema is an error and --strict
                                does not apply); --strict makes unknown
                                properties and keywords errors for files no
                                binding governs
    fix <path>...               Apply machine-applicable fixes (migrations,
                                sole-candidate suggestions) in bulk;
                                directories are walked for .nml files;
                                --dry-run prints a diff
    binding <file>...           Show the binding, grant and universe
                                governing a file (exit 0 bound, 1 unbound
                                or ambiguous, 2 error)
    explain <code>...           Explain diagnostic codes (nml explain NML2007)
    explain --list              List every diagnostic code with its headline
    limits                      Print the toolkit's published bounds (what
                                tenant content can reach, and what only
                                operator input can)
    help                        Show this help message
    version                     Show version information

OPTIONS (`nml <command> --help` lists each command's own):
    --root <dir>                The workspace root every binding glob
                                anchors under (else derived from the first
                                target within its .git fence); CI should
                                pass it
    --json                      Line-delimited JSON on stdout (one object
                                per line, `type`-discriminated); nothing on
                                stderr
    -q, --quiet                 Errors only: warnings, infos, the explain
                                hint and a verb's success lines (the
                                per-file ok or fixed lines, the closing
                                tally) are not printed; findings, a verb's
                                answer, exit codes and the counts on the
                                closing row are untouched

EXIT CODES:
    0   clean (warnings do not fail a run)
    1   findings, a universe error, an unreadable target
    2   a usage error (an unknown command, a bad flag, a missing target),
        or an invocation that contradicts the universe (--schema beside a
        governed file)

EXAMPLES:
    nml check --root . tenants/                # every .nml file under tenants/
    nml fix --check --root . .                 # CI: fail if a fix would apply
    nml fmt --check --root . .                 # CI: fail if a file would change
    nml binding --root . tenants/cu/a.flow.nml # who governs a file, and why
    nml explain NML2087                        # the full entry for a code
"
    .to_string()
}

/// Every verb's way in: `--json` armed from the raw arguments (so even
/// a usage error is a row), the one parser run against the verb's
/// [`Spec`] (a `--help` page leaves here, stdout, exit 0), then the
/// run-wide switches the invocation carries — `--quiet` and the
/// reporting budget — applied once.
fn parse_invocation(args: &[String], spec: &Spec) -> Result<Invocation, String> {
    out::detect(args);
    let inv = match Invocation::parse(args, spec)? {
        Parsed::Help(help) => out::help_page(&help),
        Parsed::Run(inv) => inv,
    };
    if inv.quiet {
        out::set_quiet();
    }
    apply_budget(&inv);
    Ok(inv)
}

/// The workspace-free verb's single file (`parse`): its [`Spec`] takes
/// exactly one positional and no `--root` (`fmt` reads a bare file at
/// its leaf the same way, through [`Reach::Leaf`]).
fn single_file(inv: &Invocation) -> PathBuf {
    PathBuf::from(&inv.targets[0])
}

/// What `nml parse` accepts, and its page ([`Spec`]).
const PARSE: Spec = Spec {
    verb: "parse",
    summary: "Parse an NML file and dump the AST as JSON (reports every error at once).",
    root: false,
    schema: false,
    strict: false,
    edit: None,
    max_findings: false,
    list: false,
    targets: "<file>",
    arity: Arity::One,
    exits: &[
        ("0", "the file parses"),
        ("1", "the file does not parse, or cannot be read"),
        ("2", "a usage error"),
    ],
    examples: &[
        "nml parse config.nml",
        "nml parse --json config.nml | jq .ast",
    ],
};

/// Dump the AST as JSON.
///
/// **Number encoding** (the one shape worth knowing before consuming this
/// output): values are externally tagged, so a number is always
/// `{"Number": …}` and can never be confused with `{"String": …}`. The
/// payload is a JSON number when the value was written in **integer
/// form** and fits `i64`/`u64`; it is the **exact decimal digits as a
/// string** otherwise. The rule is form-based, not value-based: `8080.0`
/// is integral and small, yet emits `{"Number": "8080.0"}` because the
/// written scale is part of the value. Strings therefore cover fraction
/// forms (scale preserved) and integers beyond `u64`.
///
/// Strings appear exactly where a JSON number would be lossy: most
/// readers silently truncate past 2^53 and cannot represent 128-bit
/// integers at all, so an exact string is the only encoding that survives
/// the round trip. `str::parse` recovers every value exactly.
///
/// **Duration encoding** (RFC 0017): a duration literal emits
/// `{"Duration": {"magnitude": 30, "unit": "s"}}` — the authored
/// magnitude and the unit's source suffix, faithful to the source
/// spelling (never rescaled). Consumers comparing durations across units
/// must compare totals, not pairs.
///
/// Not workspace-aware (A5, owner's decision pending): reads the path as
/// given — an operator-only verb over untrusted trees; only `check`,
/// `validate`, `fix` and `binding` are tenant-safe.
fn cmd_parse(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &PARSE)?;
    let path = single_file(&inv);
    let source = read_file(&path, &leaf_under_parent(&path)?)?;

    // A workspace-free verb names the file as typed: it has no key.
    let file = parse_or_report_all(None, &path, &path.display().to_string(), &source)?;
    if out::json() {
        // ONE `parse` row carrying the AST as data, then the run's
        // closing row; the pretty dump below is the human form.
        let ast = serde_json::to_value(&file).map_err(|e| format!("serialization error: {e}"))?;
        out::emit(&serde_json::json!({
            "type": "parse",
            "file": path.display().to_string(),
            "ast": ast,
        }));
        return Ok(());
    }
    // Streamed straight to stdout: `to_string_pretty` built the whole
    // document (70 bytes per input byte on dense input) before
    // printing a byte. Same bytes, no retained buffer.
    use std::io::Write as _;
    out::flush_err();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    serde_json::to_writer_pretty(&mut out, &file).map_err(|e| {
        if e.is_io() {
            format!("write error: {e}")
        } else {
            format!("serialization error: {e}")
        }
    })?;
    out.write_all(b"\n")
        .and_then(|()| out.flush())
        .map_err(|e| format!("write error: {e}"))?;
    Ok(())
}

/// The workspace verbs' one door (`check`, `validate`, `fix`): the
/// root is fixed once — `--root`, else derived from the
/// FIRST target — a target outside it is refused BEFORE the walk (the
/// invocation is wrong before the content is), a universe that cannot
/// be trusted is refused before any argument is expanded, and every
/// directory argument expands to
/// the kernel's one enumeration (`Workspace::expand_targets`): `nml
/// check --root . tenants/` is what a first-time operator types, and
/// no two verbs can disagree about what a directory names. An argument
/// list that names no `.nml` file fails the run, naming it.
fn open_targets(inv: &Invocation) -> Result<(workspace::Workspace, workspace::Expanded), String> {
    let ws = workspace::Workspace::open(inv.root.as_deref(), &inv.targets)?;
    let expanded = admit_targets(&ws, inv)?;
    Ok((ws, expanded))
}

/// The door and the expansion once the universe is open: a universe
/// that cannot be trusted refuses the run here
/// ([`refuse_untrusted_universe`]), else every directory argument
/// expands to the kernel's enumeration. `fix` walks through by hand: on
/// a refusal it tallies the edits the universe's rows carry in its own
/// inputs before failing the run.
pub(crate) fn admit_targets(
    ws: &workspace::Workspace,
    inv: &Invocation,
) -> Result<workspace::Expanded, String> {
    refuse_untrusted_universe(ws, &inv.targets[0])?;
    ws.expand_targets()
}

/// The gate over skipped content: for every DIRECTORY named
/// on the command line (`dirs` — the keys `open_targets` expanded, so
/// nothing is classified twice), the `.nml` content the walk left out of
/// its enumeration by policy under it is reported through the
/// locationless reporter (`Workspace::unjudged_under`) — before the
/// targets run, like the universe's own notes — and the count of
/// error-severity rows is returned for the verb's verdict. A file target
/// gates nothing: the file itself is judged.
pub(crate) fn report_unjudged(
    ws: &workspace::Workspace,
    dirs: &[nml_validate::workspace::SourceKey],
) -> usize {
    let Some(first) = dirs.first() else {
        return 0;
    };
    // Every row names its key; the fallback is the gate's first
    // directory, for the shape the reporter takes.
    report_rows(ws, first.dir_label(), &ws.unjudged_under(dirs))
}

/// The set-run driver `check` and `validate` share: the files
/// [`open_targets`] expanded run through the verb's per-file pipeline
/// ([`run_targets`]), the gate over skipped content has its say
/// ([`gated`]), and a run the operator asked of a SET — a directory
/// named, or more than one path (`Expanded::set`) — ends with ONE
/// closing line when every file passed: `checked N file(s): N ok`
/// (`validated …`), on stdout — the brief success output clig.dev asks
/// for where a 5,002-file run said nothing. A single file's own `ok`
/// line is its whole verdict; a failing set-run keeps its `error: K of
/// N file(s) failed` (or the gate's sentence) as the one closing line,
/// so success and failure each end in exactly one line. Never under
/// `--json` (the `summary` row carries `targets` and `errors`) and never
/// under `-q` (success output). Plain digits, as every count prints.
fn run_set(
    ws: &workspace::Workspace,
    expanded: &workspace::Expanded,
    unjudged: usize,
    past: &str,
    one: impl Fn(&workspace::Workspace, &str) -> Result<(), String>,
) -> Result<(), String> {
    gated(run_targets(&expanded.files, |f| one(ws, f)), unjudged)?;
    if expanded.set && !out::json() && !out::quiet() {
        let n = expanded.files.len();
        out::say(format_args!("{past} {n} file(s): {n} ok"));
    }
    Ok(())
}

/// A walking verb's verdict with the gate's: content the walk skipped
/// fails the run like a failing target does.
fn gated(run: Result<(), String>, unjudged: usize) -> Result<(), String> {
    joined(run, skipped_sentence(unjudged))
}

/// The gate's one sentence over skipped content, `None` when nothing was.
fn skipped_sentence(unjudged: usize) -> Option<String> {
    (unjudged > 0).then(|| format!("{unjudged} skipped path(s) hold content no verb judged"))
}

/// A run's verdict joined with a gate's: the gate's sentence fails the
/// run like a failing target does, and both are said when both apply.
fn joined(run: Result<(), String>, gate: Option<String>) -> Result<(), String> {
    match (run, gate) {
        (Ok(()), None) => Ok(()),
        (Err(e), None) => Err(e),
        (Ok(()), Some(s)) => Err(s),
        (Err(e), Some(s)) => Err(format!("{e}; {s}")),
    }
}

/// The multi-target driver every checking verb shares, over the files
/// [`open_targets`] expanded (`fmt` runs its leaf reach through it
/// too): each runs its unchanged single-file pipeline; a single file reports exactly as before, a failing one of
/// many reports as `error: <target>: <why>` and the run continues,
/// exiting 1 if any failed.
fn run_targets(targets: &[String], one: impl Fn(&str) -> Result<(), String>) -> Result<(), String> {
    out::set_targets(targets.len());
    if let [target] = targets {
        return one(target);
    }
    let mut failed = 0usize;
    for target in targets {
        if let Err(e) = one(target) {
            // A per-target error that already names its target (an
            // absent file's `<target>: no such file`, a read failure's
            // `failed to read <target>: …`) is not prefixed twice (the
            // read shape would otherwise come out as `<target>: failed to
            // read <target>: …`).
            if names_target(&e, target) {
                out::error_line(&e, 1, "target");
            } else {
                out::error_line(&format!("{target}: {e}"), 1, "target");
            }
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!("{failed} of {} file(s) failed", targets.len()));
    }
    Ok(())
}

/// A universe that cannot be trusted — a truncated walk, a live manifest
/// or project config that failed to load — validates and rewrites
/// NOTHING: its word is stated once ([`report_universe_notes`]) and the
/// run fails before any target is opened or any argument is expanded
/// (E28, E35). Every checking verb refuses through here, so `fix` says
/// what `check` says on an EACCES prefix too — never an OS error from a
/// walk it should not have started. (`binding` states the universe
/// through the same printer and answers anyway: its exit already
/// follows the errors it reported.)
pub(crate) fn refuse_untrusted_universe(
    ws: &workspace::Workspace,
    fallback: &str,
) -> Result<(), String> {
    match report_universe_notes(ws, fallback) {
        0 => Ok(()),
        errors => Err(format!("{errors} error(s)")),
    }
}

/// Whether a per-target error already spells its target — as its prefix
/// (`<target>: no such file or directory`) or right after the reader's
/// `failed to read ` (every `Workspace::read_target` failure) — so
/// `run_targets` never prints the spelling twice.
fn names_target(error: &str, target: &str) -> bool {
    error.starts_with(target)
        || error
            .strip_prefix("failed to read ")
            .is_some_and(|rest| rest.starts_with(target))
}

/// A directory that reached a per-file pipeline: every directory the
/// kernel's walk entered was expanded by [`open_targets`] before this,
/// so one that arrives here is one the walk did NOT enter — behind a
/// link the universe follows only in an open context (a closed binding
/// rejected it above, NML2083), or inside a denied unit (its NML2089
/// printed above and failed the target). Refused in its own words
/// rather than read as a file — by every per-file pipeline, `fix`'s
/// included (it used to read the directory and print the OS's `Is a
/// directory`).
pub(crate) fn refuse_directory(
    resolved: &nml_validate::workspace::Resolved<'_>,
    path: &Path,
) -> Result<(), String> {
    if resolved.kind == Some(nml_validate::workspace::EntryKind::Dir) {
        return Err(workspace::directory_not_entered(path));
    }
    Ok(())
}

/// An absent target, as the KERNEL saw it: the walk minted
/// the key and found no leaf (`Resolved.kind == None`) — no OS error
/// leaks, and nothing was probed by path before the resolution.
fn absent(path: &Path) -> String {
    format!("{}: no such file or directory", path.display())
}

/// What `nml validate` accepts, and its page ([`Spec`]).
const VALIDATE: Spec = Spec {
    verb: "validate",
    summary: "Validate NML files for duplicate declarations and unresolved references \
              (symbols only — run nml check for schema validation); directories are \
              walked for .nml files.",
    root: true,
    schema: false,
    strict: false,
    edit: None,
    max_findings: true,
    list: false,
    targets: "<path>...",
    arity: Arity::Many,
    exits: &[
        ("0", "clean (warnings report but do not fail)"),
        (
            "1",
            "a finding, a universe error (NML2081, NML2087–NML2089), a binding that cannot build its validator (NML2091), a rejected path (NML2083), an unreadable target, a directory naming no .nml file, or .nml content a directory walk skipped (NML2090)",
        ),
        ("2", "a usage error"),
    ],
    examples: &[
        "nml validate --root . tenants/cu/flows/a.flow.nml",
        "nml validate --root . tenants/          # every .nml file under tenants/",
    ],
};

fn cmd_validate(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &VALIDATE)?;
    let (ws, expanded) = open_targets(&inv)?;
    let unjudged = report_unjudged(&ws, &expanded.dirs);
    run_set(&ws, &expanded, unjudged, "validated", validate_one)
}

fn validate_one(ws: &workspace::Workspace, file_arg: &str) -> Result<(), String> {
    let path = PathBuf::from(file_arg);
    // The universe rejects a symlinked path under a closed binding for
    // every verb, before the file is read (E26).
    let resolved = ws.resolve(&path)?;
    let error_count = report_universe(file_arg, ws, &resolved);
    if error_count > 0 {
        return Err(format!("{error_count} error(s)"));
    }
    if resolved.kind.is_none() {
        return Err(absent(&path));
    }
    refuse_directory(&resolved, &path)?;
    let source = ws.read_target(&resolved, &path)?;
    // The key IS the file's name on the wire (step 0f).
    let own = resolved.key.to_string();

    // ONE parse of the target: the AST, its schema extraction
    // and both finding sets come from the same tree; the extraction is
    // handed to the loader below in place of the text.
    let (file, own_schema, parse_diags, facet_diags) =
        nml_core::cst::parse_and_extract_split(&source);
    let file = report_parse_findings(Some(ws), &path, &own, &source, file, parse_diags)?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let mut errors = symbols.find_unresolved_references(&file);
    errors.extend(symbols.find_const_cycles());
    // `uses` clause refs are references too (RFC 0019): `validate` does
    // not compose, but its "unresolved references" contract covers the
    // header clause — same NML2059 wording as `check`'s composing path.
    errors.extend(nml_core::layers::check_uses_refs(&own, &file));
    // Schema definitions in the file get the full loader pipeline (RFC 0011):
    // reserved/duplicate definition names, `is` composition, trait usage,
    // oneof integrity, positional arity, cycles — the same findings loading
    // the file via `--schema` would report. A file with no definitions
    // contributes nothing here.
    // The covering package's directive verdicts (the kernel's judge, shared
    // with `check` and the editor) — on the file's own extraction.
    errors.extend(directive_vocabulary_rows(
        ws,
        &resolved,
        &own_schema,
        &source,
    ));
    let (schema, schema_diags) =
        nml_validate::loader::load_schema_parts([(own.as_str(), own_schema, facet_diags)]);
    errors.extend(schema_diags);
    // The same definition-side body pass `check` runs (field defaults,
    // type-shape rules, misplaced arms/field definitions) — one code path,
    // so the definition verbs can never disagree.
    if !schema.is_empty() {
        // Merge-policy findings (RFC 0019: NML2068/NML2076) arrive from
        // the loader itself — the single owner — inside schema_diags above.
        errors.extend(
            SchemaValidator::from(schema)
                .composition_checked_at_load()
                .validate_definitions(&file),
        );
    }
    if errors.is_empty() {
        ok_line("validate", &path, &resolved.key, 0, None);
        Ok(())
    } else {
        let source_map = nml_core::span::SourceMap::new(&source);
        for err in &errors {
            report(Some(ws), &path, &own, &source, &source_map, err);
        }
        // Warnings (e.g. advisory model-reference cycles) report but do not
        // fail the file — same posture as `check`.
        let error_count = errors
            .iter()
            .filter(|d| d.severity == nml_core::diagnostic::Severity::Error)
            .count();
        let warning_count = errors
            .iter()
            .filter(|d| d.severity == nml_core::diagnostic::Severity::Warning)
            .count();
        if error_count == 0 {
            ok_line("validate", &path, &resolved.key, warning_count, None);
            return Ok(());
        }
        if out::json() {
            result_line(
                "validate",
                &path,
                &resolved.key,
                error_count,
                warning_count,
                None,
            );
        }
        Err(format!("{error_count} validation error(s)"))
    }
}

/// What `nml explain` accepts, and its page ([`Spec`]).
const EXPLAIN: Spec = Spec {
    verb: "explain",
    summary: "Explain diagnostic codes offline (e.g. nml explain NML2007 — `2007` and \
              `nml2007` name the same code; several codes print one document each); \
              --list prints every code with its one-line headline.",
    root: false,
    schema: false,
    strict: false,
    edit: None,
    max_findings: false,
    list: true,
    targets: "<code>... | --list",
    arity: Arity::ManyOrList,
    exits: &[
        ("0", "every code was found"),
        (
            "1",
            "no such code (the documents of the codes that exist still print)",
        ),
        ("2", "a usage error"),
    ],
    examples: &[
        "nml explain NML2087",
        "nml explain NML2080 NML2083        # one document per code",
        "nml explain --list | grep -i symlink",
        "nml explain --json 2087 | jq -r .document",
    ],
};

/// `nml explain NML2007` — the embedded error index (offline; the same
/// source the docs render, via the same `explain_document` composer the
/// editor's `nml/explain` serves, so "the full entry" has exactly one shape).
/// `--list` prints every code with its one-line headline (grep-able; the
/// `--json` row keeps the first-paragraph summary beside the document).
/// Coverage over every code is guaranteed by the index's bidirectional CI
/// guard plus a unit test in `nml-core`.
fn cmd_explain(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &EXPLAIN)?;
    // One `explain` row per code under `--json` — `{type, code, summary,
    // document}` — then the run's closing row.
    let explain_row = |code: &str, summary: Option<String>, document: Option<String>| {
        out::emit(&serde_json::json!({
            "type": "explain",
            "code": code,
            "summary": summary,
            "document": document,
        }));
    };
    if inv.list {
        for (code, summary) in nml_core::diagnostic::explain_index() {
            if out::json() {
                explain_row(
                    code,
                    Some(summary),
                    nml_core::diagnostic::explain_document(code),
                );
            } else {
                // The headline, not the paragraph: 136 of 137 summaries ran
                // past 80 columns (the longest 2,235 characters), so the
                // list wrapped into a wall on every terminal it was read in.
                let headline = nml_core::diagnostic::explain_headline(code).unwrap_or(summary);
                out::say(format_args!("{code}  {headline}"));
            }
        }
        return Ok(());
    }
    // Lenient spelling: `nml2007`, `2007` and `NML2007` name one code —
    // the digits are what the reader copied from the log. A log names
    // several: every code prints its own document (a blank line between
    // two), one `explain` row each under `--json`; an unknown code among
    // many is its own `error` row and the run goes on, exiting 1.
    let canonical = |typed: &str| {
        let typed = typed.trim().to_ascii_uppercase();
        if !typed.is_empty() && typed.bytes().all(|b| b.is_ascii_digit()) {
            format!("NML{typed:0>4}")
        } else {
            typed
        }
    };
    let unknown = |typed: &str| {
        format!(
            "no such diagnostic code: {typed} (codes look like NML2007; see the error index, or \
             `nml explain --list`)"
        )
    };
    let mut failed = 0usize;
    for (n, typed) in inv.targets.iter().enumerate() {
        let code = canonical(typed);
        let Some(document) = nml_core::diagnostic::explain_document(&code) else {
            if inv.targets.len() == 1 {
                return Err(unknown(typed));
            }
            out::error_line(&unknown(typed), 1, "target");
            failed += 1;
            continue;
        };
        if out::json() {
            let summary = nml_core::diagnostic::explain_index()
                .into_iter()
                .find(|(c, _)| *c == code)
                .map(|(_, s)| s);
            explain_row(&code, summary, Some(document));
            continue;
        }
        if n > 0 {
            out::say_str("\n");
        }
        // The composed document is trim-ended (the sections are trimmed at
        // the splitter); terminate the terminal line explicitly.
        out::say(format_args!("{document}"));
    }
    if failed > 0 {
        return Err(format!("{failed} of {} code(s) unknown", inv.targets.len()));
    }
    Ok(())
}

/// What `nml fmt` accepts, and its page ([`Spec`]).
const FMT: Spec = Spec {
    verb: "fmt",
    summary: "Format NML files in place — canonical style, comments preserved, atomic \
              writes. A file is formatted as named; a directory is walked for .nml files \
              through the workspace universe (as --root is); --check is the CI gate. The \
              style has no options and is specified in spec/style.md, which also says how \
              to adopt it on a tree.",
    root: true,
    schema: false,
    strict: false,
    edit: Some(Edit::Fmt),
    max_findings: false,
    list: false,
    targets: "<path>...",
    arity: Arity::Many,
    exits: &[
        (
            "0",
            "every file is in canonical style now; a dry run exits 0 whether or not a file \
             would change",
        ),
        (
            "1",
            "a file that does not parse, cannot be read or written, or is refused by the universe (NML2083, NML2087, NML2089); a universe error; a directory naming no .nml file; or, under --check, a file not in canonical style, or .nml content a directory walk skipped (NML2090)",
        ),
        ("2", "a usage error"),
    ],
    examples: &[
        "nml fmt --check --root . .        # CI: fail when a file is not in canonical style",
        "nml fmt --dry-run config.nml      # show the diff, write nothing, exit 0",
        "nml fmt --root . tenants/cu/      # format every .nml file under one tenant",
    ],
};

/// `fmt`'s own field on the run's ONE closing row (`dryRun`), on every
/// path — a run refused at the door included — as `fix` carries its.
fn set_fmt_fields(dry_run: bool) {
    out::set_summary_extra(vec![("dryRun".to_string(), serde_json::json!(dry_run))]);
}

/// How `fmt` reaches its files. A bare file target is read and
/// rewritten at its LEAF, with no universe — the way rustfmt, black,
/// prettier and gofmt format a named file, and the only way a file in
/// a directory the walk cannot list (the system temp directory, with
/// its unreadable neighbours) can be formatted at all: formatting
/// consults no manifest, so a universe would buy such a file nothing
/// but a refusal. A directory target, or `--root`, opens the ONE
/// workspace door every sibling verb uses: directories expand to the
/// kernel's enumeration, a path a closed binding rejects (NML2083) is
/// never opened, and a closed universe's write lands at the KEY
/// through the parent descriptor, as `fix`'s does.
enum Reach<'a> {
    Universe(&'a workspace::Workspace),
    Leaf,
}

/// The write that lands where the read came from — one resolution
/// serving both, whichever reach ([`fmt_one`]).
type Rewrite<'a> = Box<dyn FnOnce(&str) -> Result<(), String> + 'a>;

/// Workspace-aware where a tree is named (RFC 0026's A5): see
/// [`Reach`]. `--check` is the CI gate rustfmt, black, prettier and
/// gofmt spell the same way: nothing written, exit 1 on a file not in
/// canonical style — and, through the door, on content a directory
/// walk skipped, as `fix --check` fails on it. A set run (a directory,
/// or more than one path) ends with one closing tally; a single file's
/// own line is its verdict.
fn cmd_fmt(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &FMT)?;
    let (dry_run, gate) = (inv.dry_run, inv.check);
    set_fmt_fields(dry_run);
    // The door opens for a tree: `--root`, or any target that names a
    // directory (a link to one included — the kernel then classifies it
    // and says so). Bare files stay at their leaves.
    let through_the_door = inv.root.is_some() || inv.targets.iter().any(|t| Path::new(t).is_dir());
    let changed = std::cell::Cell::new(0usize);
    let (run, n, set, unjudged) = if through_the_door {
        let (ws, expanded) = open_targets(&inv)?;
        let unjudged = if gate {
            report_unjudged(&ws, &expanded.dirs)
        } else {
            0
        };
        let reach = Reach::Universe(&ws);
        let run = run_targets(&expanded.files, |f| {
            let differs = fmt_one(&reach, f, dry_run, expanded.set)?;
            changed.set(changed.get() + usize::from(differs));
            Ok(())
        });
        (run, expanded.files.len(), expanded.set, unjudged)
    } else {
        let set = inv.targets.len() > 1;
        let run = run_targets(&inv.targets, |f| {
            let differs = fmt_one(&Reach::Leaf, f, dry_run, set)?;
            changed.set(changed.get() + usize::from(differs));
            Ok(())
        });
        (run, inv.targets.len(), set, 0)
    };
    let changed = changed.get();
    if set && !out::json() && !out::quiet() {
        // The set's one closing line, as `checked N file(s): N ok`.
        if dry_run {
            out::say(format_args!(
                "{changed} of {n} file(s) not in canonical style"
            ));
        } else {
            out::say(format_args!("formatted {changed} of {n} file(s)"));
        }
    }
    // The gate fails on a file not in canonical style and on content
    // the walk skipped; `--dry-run` alone keeps its Unix meaning, exit 0.
    let unformatted = (gate && changed > 0).then(|| {
        format!("{changed} file(s) not in canonical style — run `nml fmt` to format them")
    });
    let why: Vec<String> = unformatted
        .into_iter()
        .chain(skipped_sentence(unjudged))
        .collect();
    let gate_failure =
        (!why.is_empty()).then(|| format!("{}; nothing was written", why.join("; ")));
    joined(run, gate_failure)
}

/// One file through the formatter. Through the door: the kernel's
/// findings first (a rejected path is never opened), then the read and
/// the write through the universe's own `open_target`/`write_target`.
/// At the leaf: the parent-anchored `O_NOFOLLOW` read and the atomic
/// write the workspace-free verb uses, one resolution serving both.
/// Either way the parse goes through the one reporter (every parse
/// error, each with its code), and the canonical text is written — or,
/// on a dry run, shown as a unified diff (the verb's answer: it prints
/// under `-q` too). Returns whether the file's bytes differ from
/// canonical.
fn fmt_one(reach: &Reach<'_>, file_arg: &str, dry_run: bool, set: bool) -> Result<bool, String> {
    let path = PathBuf::from(file_arg);
    let ws = match reach {
        Reach::Universe(ws) => Some(*ws),
        Reach::Leaf => None,
    };
    // The read, and the write that lands where the read came from.
    let (source, own, write): (String, String, Rewrite<'_>) = match reach {
        Reach::Universe(ws) => {
            let resolved = ws.resolve(&path)?;
            let errors = report_universe(file_arg, ws, &resolved);
            if errors > 0 {
                return Err(format!("{errors} error(s)"));
            }
            refuse_directory(&resolved, &path)?;
            if resolved.kind.is_none() {
                return Err(absent(&path));
            }
            let own = resolved.key.to_string();
            let workspace::Opened { text, leaf } = ws.open_target(&resolved, &path)?;
            let at = path.clone();
            (
                text,
                own,
                Box::new(move |formatted: &str| ws.write_target(&resolved, &leaf, &at, formatted)),
            )
        }
        Reach::Leaf => {
            let at = leaf_under_parent(&path)?;
            let text = read_file(&path, &at)?;
            let spelled = path.clone();
            (
                text,
                path.display().to_string(),
                Box::new(move |formatted: &str| write_file_atomically(&spelled, &at, formatted)),
            )
        }
    };
    parse_or_report_all(ws, &path, &own, &source)?;
    let formatted = nml_fmt::formatter::format_source(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!(
            "{}:{}:{}: {}",
            sanitized(&path.display().to_string()),
            loc.line,
            loc.column,
            e
        )
    })?;
    let changed = formatted != source;
    if changed {
        if dry_run {
            if !out::json() {
                out::say_raw(&fix::unified_diff(&source, &formatted, &path));
            }
        } else {
            write(&formatted)?;
        }
    }
    if out::json() {
        // ONE `fmt` row per file: the file and whether its bytes differ
        // from canonical (a dry run: whether a rewrite would change them).
        out::emit(&serde_json::json!({
            "type": "fmt",
            "file": path.display().to_string(),
            "changed": changed,
        }));
        return Ok(changed);
    }
    if out::quiet() {
        return Ok(changed);
    }
    // Success output: a changed file says so on every run; an unchanged
    // one only when it is the run's whole verdict (a set's tally counts it).
    let spelled = sanitized(&path.display().to_string());
    match (changed, dry_run, set) {
        (true, false, _) => out::say(format_args!("formatted {spelled}")),
        (true, true, _) => out::say(format_args!("would format {spelled}")),
        (false, _, false) => out::say(format_args!("{spelled}: already in canonical style")),
        (false, _, true) => {}
    }
    Ok(changed)
}

struct CheckOpts {
    schema_dir: Option<PathBuf>,
    strict: bool,
}

/// What `nml check` accepts, and its page ([`Spec`]).
const CHECK: Spec = Spec {
    verb: "check",
    summary: "Parse + validate + schema check (CI-friendly): a file a workspace manifest \
              claims validates under its binding's package; directories are walked for \
              .nml files.",
    root: true,
    schema: true,
    strict: true,
    edit: None,
    max_findings: true,
    list: false,
    targets: "<path>...",
    arity: Arity::Many,
    exits: &[
        ("0", "clean (warnings report but do not fail)"),
        (
            "1",
            "a finding, a universe error (NML2081, NML2087–NML2089), a binding that cannot build its validator (NML2091), a rejected path (NML2083), an unreadable target, a directory naming no .nml file, or .nml content a directory walk skipped (NML2090)",
        ),
        (
            "2",
            "a usage error, or --schema beside a manifest-governed file (the invocation contradicts the universe)",
        ),
    ],
    examples: &[
        "nml check --root . tenants/                     # every .nml file under tenants/",
        "nml check --root . --json tenants/cu/flows/a.flow.nml | jq 'select(.type==\"summary\")'",
        "nml check --schema schemas/ --strict deploy.nml # no manifest: the flags decide",
    ],
};

fn cmd_check(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &CHECK)?;
    if let Some(dir) = &inv.schema {
        workspace::require_schema_dir(dir)?;
    }
    // The universe first (RFC 0019 item 0): the root is fixed once per
    // invocation (from the FIRST target, as `fix` does), every target's
    // governing binding is resolved under it, and a path a closed
    // binding rejects (NML2083) is never opened — the rejection is
    // byte-identical whether or not a symlink's target exists.
    let (ws, expanded) = open_targets(&inv)?;
    let opts = CheckOpts {
        schema_dir: inv.schema.clone(),
        strict: inv.strict,
    };
    let unjudged = report_unjudged(&ws, &expanded.dirs);
    run_set(&ws, &expanded, unjudged, "checked", |ws, f| {
        check_one(ws, f, &opts)
    })
}

/// `check`'s validator sink: a finding is reported through the
/// one reporter and tallied the moment it is pushed — never held.
/// Deduplication against the composed findings is the kernel's
/// (`layers::Deduped`, wrapped around this sink), the same rule the
/// editor validates through.
struct Streamed<'a> {
    ws: &'a workspace::Workspace,
    path: &'a Path,
    own: &'a str,
    source: &'a str,
    source_map: &'a nml_core::span::SourceMap,
    errors: usize,
    warnings: usize,
}

impl nml_core::diagnostic::DiagnosticSink for Streamed<'_> {
    fn push(&mut self, diag: Diagnostic) {
        report(
            Some(self.ws),
            self.path,
            self.own,
            self.source,
            self.source_map,
            &diag,
        );
        match diag.severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            _ => {}
        }
    }
}

/// The covering package's directive verdicts on a schema source (RFC 0019
/// §Merge policy, RFC 0030): the kernel's one judge, so this verb and the
/// editor report the same rows — NML5000 (with its did-you-mean), NML5001,
/// NML5002 and the NML5003 note for an undeclared sibling. Judged on the
/// file's OWN extraction, before the load's inheritance resolution copies a
/// trait's directives into every child. A source no package covers is
/// judged under no vocabulary: every directive accepted, as the language
/// guide says — and where that has a reason the author can act on (a
/// truncated universe, two or more packages that could cover the file),
/// the kernel's one note says so (`VocabularyOutcome::note`), as the
/// editor says it.
fn directive_vocabulary_rows(
    ws: &workspace::Workspace,
    resolved: &nml_validate::workspace::Resolved<'_>,
    schema: &nml_core::schema::ExtractedSchema,
    source: &str,
) -> Vec<Diagnostic> {
    let outcome = ws.discovery().vocabulary_for(&resolved.key);
    let mut rows: Vec<Diagnostic> = outcome.note().into_iter().collect();
    if let Some(vocab) = outcome.covered() {
        rows.extend(vocab.judge(&schema.models, source));
    }
    rows
}

fn check_one(ws: &workspace::Workspace, file_arg: &str, opts: &CheckOpts) -> Result<(), String> {
    let path = PathBuf::from(file_arg);
    let schema_dir = opts.schema_dir.clone();
    let strict = opts.strict;
    let resolved = ws.resolve(&path)?;
    let mut error_count = report_universe(file_arg, ws, &resolved);
    let mut warnings = 0usize;
    if error_count > 0 {
        return Err(format!("{error_count} error(s)"));
    }
    refuse_directory(&resolved, &path)?;
    // D-0d-1: a claim-governed file validates under its binding's
    // package; `--schema` beside a governing binding is a usage error.
    let judged = match workspace::judge(&resolved, schema_dir.as_deref(), &path) {
        Ok(v) => v,
        Err(workspace::Conflict(message)) => exit_usage(&message),
    };
    if resolved.kind.is_none() {
        return Err(absent(&path));
    }

    let source = ws.read_target(&resolved, &path)?;
    // The key IS the file's name on the wire (step 0f).
    let own = resolved.key.to_string();

    // ONE parse of the checked file: the AST, its schema
    // extraction and both finding sets come from the same tree. The
    // extraction is handed to the loader below in place of the text, so
    // the file is never parsed a second time while its first AST is held
    // (that double parse was the largest term of `check`'s peak memory).
    let (file, own_schema, parse_diags, facet_diags) =
        nml_core::cst::parse_and_extract_split(&source);
    let file = report_parse_findings(Some(ws), &path, &own, &source, file, parse_diags)?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let source_map = nml_core::span::SourceMap::new(&source);

    for err in symbols
        .find_unresolved_references(&file)
        .into_iter()
        .chain(symbols.find_const_cycles())
    {
        report(Some(ws), &path, &own, &source, &source_map, &err);
        error_count += 1;
    }

    // One schema universe per check (RFC 0012): the `--schema` directory's
    // sources plus the checked file itself (unless it *is* one of them). A
    // single load runs every definition pass — reserved/duplicate names,
    // `is` composition, trait usage, oneof integrity, positional arity,
    // cycles — with per-file attribution, and the composed schema then
    // types instances. A self-contained file (`model cache` above
    // `cache Foo:`) validates with no flags, and a name declared in both
    // the file and the directory is NML2009 — never a silent shadow.
    // Assembly is shared with `nml fix` (the workspace module), so the fixer
    // can never judge a file differently than this verb does. Under a
    // governing binding the instance validator is the binding's (judged
    // above); the file's own definitions still get the definition pass.
    // The covering package's directive verdicts (the kernel's judge, shared
    // with the editor) — on the file's own extraction, before it is loaded.
    for diag in directive_vocabulary_rows(ws, &resolved, &own_schema, &source) {
        report(Some(ws), &path, &own, &source, &source_map, &diag);
        match diag.severity {
            Severity::Error => error_count += 1,
            Severity::Warning => warnings += 1,
            _ => {}
        }
    }

    let named_sources = workspace::schema_universe(&path, &own, &source, schema_dir.as_ref())?;
    let (schema, schema_diags) = nml_validate::loader::load_schema_parts(workspace::schema_parts(
        &named_sources,
        &source,
        (own_schema, facet_diags),
    ));

    // Attributed findings print `path:line:col` against their declaring
    // source; a finding no single definition owns falls back to a
    // location-less line under the schema dir (or the file).
    for diag in &schema_diags {
        let attributed = diag
            .source
            .as_deref()
            .and_then(|name| named_sources.iter().find(|(n, _, _)| n == name));
        match attributed {
            Some((name, src_path, text)) => {
                report(
                    Some(ws),
                    src_path,
                    name,
                    text,
                    &nml_core::span::SourceMap::new(text),
                    diag,
                );
            }
            None => {
                // Through the one locationless reporter (sanitized, JSON-
                // aware — never the directory name raw with `Display`'s
                // byte-span suffix, A1).
                let fallback = match &schema_dir {
                    Some(sd) => sd.display().to_string(),
                    None => path.display().to_string(),
                };
                report_locationless(ws, &fallback, diag);
            }
        }
        if matches!(diag.severity, Severity::Error) {
            error_count += 1;
        } else if matches!(diag.severity, Severity::Warning) {
            warnings += 1;
        }
    }

    // `--strict` promises enforcement; with an empty schema universe there
    // is nothing to enforce, and silently degrading to parse-only checking
    // is how a CI pipeline points at the wrong path and stays green
    // forever. Fail the *invocation*, naming the actual mistake. A
    // binding-supplied validator always has something to enforce.
    if strict && schema.is_empty() && judged.is_none() {
        // The invocation's mistake, not the file's: exit 2 like the
        // `--schema` conflict, so a CI script that pointed `--strict` at
        // a schema-less path learns it is the command that is wrong.
        //
        // In a CLOSED universe the cause is a different one and the
        // remedy is the operator's: the manifests are there, they simply
        // claim other files. Saying "no --schema directory given and
        // none declared in the file" there named neither the cause nor
        // anything the reader could do, in the one workspace where a
        // stray unclaimed file is the likely mistake.
        let universe = ws.discovery().universe();
        if universe.is_closed() {
            exit_usage(&format!(
                "--strict has nothing to enforce on {}: no binding claims it in the closed \
                 universe ({} manifest(s) discovered), so no package supplies its schema — \
                 add a `files` glob that claims it (an operator change), or drop --strict; \
                 run `nml binding {}` to see the claim",
                sanitized(&own),
                universe.workspace_claims(),
                sanitized(&own),
            ));
        }
        exit_usage(
            "--strict has nothing to enforce: no schema definitions found (no --schema \
             directory given and none declared in the file)",
        );
    }

    {
        // RFC 0019: compose `uses` stacks before validation (default on)
        // under the file's governing binding — the grant provider is the
        // shared resolution core, so NML2064/2065 are live here exactly
        // as in the editor. Blocks that compose validate their RESOLVED
        // body (an overlay alone is deliberately partial); everything
        // else validates as authored. The pass runs even with no schema
        // universe: NML2059/2061/2062/2077 are structural, not
        // schema-dependent. The grant is the one the resolution already
        // carries (`Resolved::grant`), never a second lookup. The
        // validator is built FIRST and its index
        // shared with the layers engine — one index build, zero schema
        // clones, and the merge-policy pass and the validator can never
        // see different schemas.
        let validator = match judged {
            // The binding's, from the kernel's table, under the binding's
            // OWN strictness: `--strict` does not apply to a bound file —
            // the editor cannot apply it either, and the two front ends
            // give one verdict — and says so once per run where it would
            // have tightened one.
            Some(v) => {
                if strict {
                    strict_does_not_apply(&resolved, file_arg);
                }
                Some(v)
            }
            None => (!schema.is_empty()).then(|| {
                let mut v = SchemaValidator::from(schema).composition_checked_at_load();
                if strict {
                    v = v.strict();
                }
                std::sync::Arc::new(v)
            }),
        };
        let empty_index = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
        let index = validator.as_ref().map_or(&empty_index, |v| v.index());
        let composed = nml_core::layers::compose_file(index, &own, &file, &resolved.grant);
        for diag in &composed.diagnostics {
            report(Some(ws), &path, &own, &source, &source_map, diag);
            if matches!(diag.severity, Severity::Error) {
                error_count += 1;
            } else if matches!(diag.severity, Severity::Warning) {
                warnings += 1;
            }
        }
        // The composed findings' keys the validator's must meet on one
        // key — the kernel's rule (`ComposedFile::dedup_seed`): only
        // while the file composes.
        let seed = composed.dedup_seed(&own);
        let validation_file = composed.validation_file;

        if let Some(validator) = &validator {
            // Definition composition is covered by the single load above —
            // instance-only here, so no finding is ever reported twice
            // across passes; and one home per finding within this pass — a
            // resolved overlay body carries clones of base entries at their
            // authored spans, so a base defect would otherwise report once
            // as authored and once per overlay. Identical (code, span,
            // message, source) quadruples collapse to one. SEEDED with the
            // composed diagnostics already reported above: the merge emits
            // some validator-shaped findings itself (a bogus `as` the
            // composed view would otherwise swallow, NML2051), and a
            // non-`uses` base declaration's raw validation re-derives the
            // same finding at the same span — the LSP and `nml fix` seed
            // this way too, and an unseeded set printed the pair twice
            // here. Keyed IN this file (`finding_key_in`): the validator's
            // findings are unstamped and must meet the merge's stamped
            // twins on one key.
            // STREAMED: the validator pushes each finding into
            // the reporter as it is derived — deduplicated by the
            // kernel's one rule, reported and tallied at once, never
            // collected — so a document yielding a million findings
            // costs the printing budget, not a million held findings.
            let mut sink = Streamed {
                ws,
                path: &path,
                own: &own,
                source: &source,
                source_map: &source_map,
                errors: 0,
                warnings: 0,
            };
            let mut deduped = nml_core::layers::Deduped::new(&mut sink, &own, seed);
            validator.validate_into(validation_file.as_ref().unwrap_or(&file), &mut deduped);
            error_count += sink.errors;
            warnings += sink.warnings;
        }
    }

    let decl_count = file.declarations.len();
    if error_count == 0 {
        ok_line("check", &path, &resolved.key, warnings, Some(decl_count));
        Ok(())
    } else {
        if out::json() {
            result_line(
                "check",
                &path,
                &resolved.key,
                error_count,
                warnings,
                Some(decl_count),
            );
        }
        Err(format!("{error_count} error(s)"))
    }
}

/// What `nml binding` accepts, and its page ([`Spec`]).
const BINDING: Spec = Spec {
    verb: "binding",
    summary: "Show the binding, grant and universe governing a file: the key, the root and \
              how it was fixed, the matched glob, the effective layers: grant and every \
              inert input on its path.",
    root: true,
    schema: false,
    strict: false,
    edit: None,
    max_findings: false,
    list: false,
    targets: "<file>...",
    arity: Arity::Many,
    exits: &[
        ("0", "the file is bound and the run reported no error"),
        (
            "1",
            "unbound or ambiguously claimed, or any error the run reported — a universe \
             error (NML2081, NML2087–NML2089) or a binding that cannot build its validator (NML2091) \
             included, even where a binding stands (the worst \
             across targets)",
        ),
        (
            "2",
            "a usage error, a directory target, the root cannot be derived, or the file is outside it",
        ),
    ],
    examples: &[
        "nml binding --root . tenants/cu/flows/a.flow.nml",
        "nml binding --root . --json shared/x.flow.nml | jq .claimants",
    ],
};

/// `nml binding <file>` (RFC 0019 item 0, step 0d — the minimal verb):
/// the universe a file resolves in and the binding that governs it, as
/// a user reads it — aligned `key value` lines on stdout, grep-style
/// exit codes (0 bound, 1 unbound or ambiguous, 2 error), the same rule
/// indices NML2065 names, and every inert input on the file's path under
/// `notes`. The universe's own word — its errors, or its layout notes —
/// is stated ONCE per run before the first block, on stderr, exactly as
/// every verb states it ([`report_universe_notes`]); a block carries what
/// bears on ITS key only. Many targets share one universe (one row block
/// per target, blank-line separated; the exit code is the WORST across
/// targets); `--json` emits the universe's rows once, then one `binding`
/// row per target. `--ref <path>` is plan item 5.
/// The `layers` object of a `binding` row — the kernel's one spelling
/// ([`LayersWire`]), the editor's `nml/schemaInfo` carries the same.
fn layers_value(layers: LayersWire) -> serde_json::Value {
    serde_json::to_value(layers).unwrap_or(serde_json::Value::Null)
}

fn cmd_binding(args: &[String]) -> Result<(), String> {
    let inv = parse_invocation(args, &BINDING)?;
    let ws = match workspace::Workspace::open(inv.root.as_deref(), &inv.targets) {
        Ok(ws) => ws,
        Err(e) => exit_usage(&e),
    };
    out::set_targets(inv.targets.len());
    // The universe's word once, before any block — the verb answers
    // under an untrusted universe too (the exit follows the errors).
    report_universe_notes(&ws, &inv.targets[0]);
    let mut worst = 0;
    for (n, file_arg) in inv.targets.iter().enumerate() {
        if n > 0 && !out::json() {
            out::say_str("\n");
        }
        worst = worst.max(binding_one(&ws, file_arg));
    }
    // The exit follows the closing row like every verb's: a run that
    // reported an error-severity finding — a universe error such as
    // NML2088 included — exits 1 even where a binding still stands.
    let exit = if out::errors_reported() > 0 {
        worst.max(1)
    } else {
        worst
    };
    finish(exit)
}

fn binding_one(ws: &workspace::Workspace, file_arg: &str) -> i32 {
    let path = PathBuf::from(file_arg);
    let resolved = match ws.resolve(&path) {
        Ok(r) => r,
        Err(e) => exit_usage(&e),
    };
    // A directory is not a file `binding` can answer for: said so, exit
    // 2 (the invocation), never answered as if it were an unbound file.
    let universe = ws.discovery().universe();
    // A typed LINK to a directory is one too, in an open universe (the
    // walk follows a developer's own link; a closed universe halts at
    // the link with NML2083 and never observes its target — E26), as
    // `check` refuses it at its open.
    let linked_dir = || {
        !universe.is_closed()
            && resolved.kind == Some(nml_validate::workspace::EntryKind::Symlink)
            && std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir())
    };
    if resolved.kind == Some(nml_validate::workspace::EntryKind::Dir) || linked_dir() {
        exit_usage(&format!(
            "`{}` is a directory — nml binding takes files; name a file under it",
            path.display()
        ));
    }
    // The KEY's own notes — the inert inputs on its chain (the
    // universe's word was stated once, above) — the kernel's findings,
    // and, for a path the walk VERIFIED and found no leaf for, a
    // warning that the binding shown is the one that WOULD govern the
    // path, so an absent file is never mistaken for an unbound one. A
    // path the kernel rejected (NML2083) has no verified leaf either,
    // but its finding is the whole story: no absence is claimed for it
    // (the target of a link is never observed — E26).
    let mut notes: Vec<Diagnostic> = ws.discovery().inert_notes_for(&resolved.key);
    notes.extend(resolved.findings.iter().cloned());
    let rejected = resolved
        .findings
        .iter()
        .any(|d| d.severity == Severity::Error);
    let absent = resolved.kind.is_none() && !rejected;
    if absent {
        notes.push(
            Diagnostic::warning("no such file — the binding shown is what WOULD govern this path")
                .with_source(resolved.key.as_str().to_string()),
        );
    }
    // The kernel's symlink verdict, SURFACED: an open universe follows
    // a developer's own link and says so — the key names the target,
    // the note names the linked component of the root-relative
    // spelling (1-based). A closed universe never reaches this: its
    // walk halts at the link with NML2083 above.
    if let SymlinkVerdict::Through(i) = resolved.via_symlink {
        notes.push(
            Diagnostic::info(format!(
                "resolved through a symlink at component {} of the root-relative path — \
                 followed (open universe); a closed binding would reject it (NML2083)",
                i + 1
            ))
            .with_source(resolved.key.as_str().to_string()),
        );
    }
    // The closing row's `errors`/`warnings` count what this block
    // reports (a `binding --json` that said `errors: 0` while exiting 1
    // on a kernel finding would lie), and the run's explain hint names
    // the first coded row as every verb's does; under `--quiet` only
    // the errors among them are shown.
    for note in &notes {
        note_hint(note);
        out::tally(note);
    }
    let notes: Vec<Diagnostic> = notes
        .into_iter()
        .filter(|n| !out::quiet() || n.severity == Severity::Error)
        .collect();
    if out::json() {
        return binding_json(ws, &resolved, &universe, &notes, absent);
    }
    let mut out = String::new();
    let mut line = |k: &str, v: &str| {
        out.push_str(&format!("{k:<9} {}\n", sanitized(v)));
    };
    // An absent file says so on the `file` line itself: under `-q` the
    // warning above is silent, and the screen must never read as an
    // existing file's (the exit is unchanged — the question asked is
    // what WOULD govern the path).
    if absent {
        line("file", &format!("{}  (absent)", resolved.key.as_str()));
    } else {
        line("file", resolved.key.as_str());
    }
    line(
        "root",
        &format!(
            "{}  {}",
            workspace::display_path(ws.discovery().root().path()),
            ws.origin_tag()
        ),
    );
    let exit = match &resolved.governing {
        Governing::Bound { claimant, step } => {
            line(
                "binding",
                &format!(
                    "{}   {} ({})",
                    claimant.binding.name,
                    claimant.claim.identity().render(),
                    claimant.claim.manifest_label()
                ),
            );
            let anchor = claimant.anchor.dir_label();
            let step = step.label();
            line(
                "anchor",
                &format!(
                    "{anchor}   matched files[{}] = {:?}   ({step})",
                    claimant.glob, claimant.binding.files[claimant.glob]
                ),
            );
            match &claimant.binding.layers {
                Some(grant) => {
                    line("layers", "granted");
                    for rule in grant.rules() {
                        line("", &rule);
                    }
                }
                None => line("layers", nml_validate::workspace::Grant::DENIED),
            }
            0
        }
        Governing::Ambiguous(claimants) => {
            // The summary the kernel's error finding opens with — one
            // rendering, one claimant order; `notes` below carries the
            // finding itself, as `check` prints it.
            line(
                "binding",
                &format!(
                    "AMBIGUOUS — {}",
                    nml_validate::workspace::ambiguous_claim_summary(claimants)
                ),
            );
            line("layers", "none — the file is denied (NML2087)");
            1
        }
        Governing::Unbound => {
            if universe.is_closed() {
                line(
                    "binding",
                    &format!(
                        "none — closed universe ({} manifest(s) discovered); no files glob \
                         claims this file",
                        universe.workspace_claims()
                    ),
                );
                line("layers", nml_validate::workspace::Grant::DENIED);
            } else {
                line(
                    "binding",
                    "none — open universe (no manifest within the fence); composition permitted",
                );
            }
            1
        }
    };
    for (i, note) in notes.iter().enumerate() {
        let source = note.source.as_deref().unwrap_or(resolved.key.as_str());
        line(
            if i == 0 { "notes" } else { "" },
            &universe_note_line(source, ws.manifest_location(note), note, false),
        );
    }
    out::say_str(&out);
    exit
}

/// The binding row as FACTS — the kernel's tags (`RootOrigin::tag`,
/// `BindingStep::tag`, `ClaimClass::tag`, the raw content hash), never
/// the human sentence. Shape:
/// `{type:"binding", file, absent, root:{path, origin, fence, shadowed},
///   universe:"closed"|"open", closure, manifests, truncatedUnits,
///   governing:"bound"|"unbound"|"ambiguous", binding:{name,
///   package, contentHash, class, manifest, anchor, glob:{index,
///   pattern}, step} | null, layers:{granted:false} | {granted:true,
///   allowRefs[], denyRefs[], maxStackDepth}, claimants:[…] (ambiguous
///   only), notes:[diagnostic rows]}`.
fn binding_json(
    ws: &workspace::Workspace,
    resolved: &nml_validate::workspace::Resolved<'_>,
    universe: &nml_validate::workspace::Universe<'_>,
    notes: &[Diagnostic],
    absent: bool,
) -> i32 {
    use serde_json::json;
    let claimant_value = |c: &nml_validate::workspace::Claimant<'_>| {
        json!({
            "name": c.binding.name,
            "package": c.claim.name(),
            "contentHash": c.claim.identity().content_hash,
            "class": c.claim.class().tag(),
            "manifest": c.claim.manifest_label(),
            "anchor": c.anchor.dir_label(),
            "glob": {"index": c.glob, "pattern": c.binding.files[c.glob]},
        })
    };
    let notes: Vec<serde_json::Value> = notes
        .iter()
        .map(|d| locationless_value(ws, d.source.as_deref().unwrap_or(resolved.key.as_str()), d))
        .collect();
    let (governing, binding, layers, claimants, exit) = match &resolved.governing {
        Governing::Bound { claimant, step } => {
            let mut b = claimant_value(claimant);
            b["step"] = json!(step.tag());
            let layers = layers_value(LayersWire::of(claimant.binding.layers.as_ref()));
            ("bound", b, layers, json!(null), 0)
        }
        Governing::Ambiguous(cs) => (
            "ambiguous",
            json!(null),
            layers_value(LayersWire::context(false)),
            json!(cs.iter().map(claimant_value).collect::<Vec<_>>()),
            1,
        ),
        Governing::Unbound => (
            "unbound",
            json!(null),
            layers_value(LayersWire::context(!universe.is_closed())),
            json!(null),
            1,
        ),
    };
    // The universe facts the `summary` row carries ride this row too:
    // `closure` and the budget units the walk stopped inside — a
    // `binding` row that said `closed` for a universe with a denied
    // tenant in it would read as a whole one.
    let truncated_units: Vec<serde_json::Value> = workspace::truncated_units(universe)
        .into_iter()
        .map(|(unit, stop, why)| json!({"unit": unit, "stop": stop, "why": why}))
        .collect();
    out::emit(&json!({
        "type": "binding",
        "file": resolved.key.as_str(),
        // The human `(absent)` tag's wire twin: the path the walk
        // verified and found no leaf for (the note is silent under
        // `--quiet`, the row must not be).
        "absent": absent,
        "root": ws.root_facts().row(),
        "universe": universe.state().label(),
        "closure": universe.closure.tag(),
        "manifests": universe.workspace_claims(),
        "truncatedUnits": truncated_units,
        "governing": governing,
        "binding": binding,
        "layers": layers,
        "claimants": claimants,
        "notes": notes,
    }));
    exit
}

/// The per-target success line — SANITIZED (A1: a raw
/// `println!("{}: ok", path.display())` would print a hostile filename's
/// ESC and LF bytes to stdout, so a tenant-named file could forge a
/// second `: ok` line in the operator's CI log) — or the JSON `result` row.
/// `validate` says what it did not do: symbols only, no schema. Silent
/// under `-q`: success output is the non-essential output the flag
/// suppresses (clig.dev), never a finding, never a `--json` row.
fn ok_line(
    verb: &str,
    path: &Path,
    key: &nml_validate::workspace::SourceKey,
    warnings: usize,
    declarations: Option<usize>,
) {
    if out::json() {
        result_line(verb, path, key, 0, warnings, declarations);
        return;
    }
    if out::quiet() {
        return;
    }
    let path = sanitized(key.as_str());
    match declarations {
        Some(n) => out::say(format_args!("{path}: ok ({n} declaration(s))")),
        None => out::say(format_args!(
            "{path}: ok (symbols only — run nml check for schema validation)"
        )),
    }
}

/// `{type:"result", verb, target, key, ok, errors, warnings,
/// declarations}` — one per target, always the LAST row for that target.
fn result_line(
    verb: &str,
    path: &Path,
    key: &nml_validate::workspace::SourceKey,
    errors: usize,
    warnings: usize,
    declarations: Option<usize>,
) {
    out::emit_verdict(&serde_json::json!({
        "type": "result",
        "verb": verb,
        "target": path.display().to_string(),
        "key": key.as_str(),
        "ok": errors == 0,
        "errors": errors,
        "warnings": warnings,
        "declarations": declarations,
    }));
}

/// The ONLY read of an operator file in this crate outside the universe
/// (A3's structural pin): the workspace-free verb's (`parse`) input,
/// and `fmt`'s at its leaf reach — read with the LEAF SAFETY a verb
/// with no universe can still have (an open universe's target read
/// opens at the same resolved leaf).
///
/// There is no principled root for an absolute operator path, and
/// deriving one is not an option: `WorkspaceRoot::derive` refuses
/// `/tmp/x.nml` on macOS outright (`/tmp` is itself a link and no `.git`
/// fences the walk — `RootError::UnfencedSymlink`). What IS achievable
/// with no universe is the leaf's own safety: the file is opened
/// **beneath its own parent directory** (`open_beneath`: `O_NOFOLLOW`
/// on the leaf, then `fstat` must say regular file), so
///
/// * a **symlinked** final component is followed ONCE, to the file it
///   names ([`leaf_under_parent`] resolves it, once per INVOCATION: the
///   caller resolves and hands the [`LeafAt`] to the read and to the
///   write), and the open is
///   anchored at that file's own parent — the link is never the thing
///   read, and a leaf that becomes a link between the resolution and
///   the open is refused there;
/// * a **FIFO** never blocks the reader (`nml parse <fifo>` would hang
///   forever, and `nml fmt <fifo>` with it);
/// * a **character device** is never streamed (`nml parse /dev/zero`
///   grew without bound).
///
/// The parent is the operator's typed prefix and is opened BY PATH:
/// links in it are followed, exactly as `rustfmt`, `black` and
/// `prettier` follow them. This is a leaf rule, not a universe.
fn read_file(path: &Path, at: &LeafAt) -> Result<String, String> {
    // The kernel's one reader at the resolved leaf, under the same
    // take-cap-plus-one discipline as every other target read:
    // `parse`/`fmt` read whole, and one 256 MiB file reached 20 GB
    // resident before the first diagnostic printed.
    read_beneath(
        &at.dir,
        &[at.name.as_str()],
        workspace::MAX_TARGET_BYTES,
        "a file",
    )
    .map_err(|e| {
        let why = match e {
            ReadError::Open(OpenError::NotRegular { dir: true, .. }) => {
                "is a directory".to_string()
            }
            ReadError::Open(e @ OpenError::Symlink { .. }) => leaf_advice(e),
            ReadError::Open(e) => e.to_string(),
            e => e.to_string(),
        };
        format!("failed to read {}: {why}", workspace::message_path(path))
    })
}

/// The kernel states the fact, the CLI adds the advice (E35): a link
/// refused at a workspace-free verb's leaf gets the one sentence only
/// this front end can give — the verbs with no universe have no
/// authority to decide whether a link may be followed, so they read
/// only what the operator typed. An open universe's target read
/// (`workspace::Workspace::open_target`) speaks it too: it opens at the
/// same resolved leaf.
pub(crate) fn leaf_advice(e: nml_validate::workspace::OpenError) -> String {
    match e {
        nml_validate::workspace::OpenError::Symlink { .. } => format!(
            "{e} — the leaf became a link between its resolution and the open; nothing was \
             read — run again"
        ),
        e => e.to_string(),
    }
}

/// The operator's typed leaf, resolved ONCE per invocation: the
/// directory to anchor under and the leaf's own name — a typed link
/// followed once, to the file it names. The read opens here and the
/// write lands here, so the typed link re-pointed between the two
/// changes nothing: the file that was read is the file that is written.
/// (A write that resolved the link a second time would land the read
/// file's text in the NEW target after a re-point during a long format
/// — measured deterministically.)
pub(crate) struct LeafAt {
    pub(crate) dir: PathBuf,
    pub(crate) name: String,
}

/// A path split into [`LeafAt`] (the directory to anchor under, the
/// leaf name). The parent is the operator's typed prefix — empty (a
/// bare file name) means the working directory. A spelling with no
/// file name (`a/`, `..`, `/`) names no leaf and is refused before any
/// syscall.
///
/// A typed leaf that is a SYMLINK is resolved here, once, to the file
/// it names: the anchor becomes the TARGET's own
/// parent and the leaf its own name, so the `O_NOFOLLOW` open in
/// [`read_file`] and the temp-and-rename write in
/// [`write_file_atomically`] both land on the file the link points at,
/// and the link itself is never opened as content and never replaced.
/// That is what `rustfmt`, `gofmt`, `black` and `prettier` do with a
/// linked file (they format the target in place and leave the link —
/// measured), and what `check link.nml` already does on the way in: an
/// open universe follows the operator's own links by design — wherever
/// the link points, inside or outside the derived root. The resolved
/// leaf is a regular file's own name, never a link: the typed link
/// re-pointed after this resolution is never consulted again (one
/// resolution serves the read and the write), and the resolved file itself
/// swapped for a link is refused AT the open or the write rather than
/// followed a second time; a dangling link resolves to nothing and is
/// refused before anything is read or created.
pub(crate) fn leaf_under_parent(path: &Path) -> Result<LeafAt, String> {
    // Classified WITHOUT a trailing separator: `link/` names the
    // link's target directory to `lstat`, so the link went unresolved
    // and the `O_NOFOLLOW` open blamed a race that never happened;
    // resolved, the open refuses the directory in its own words.
    let typed = split_typed(path)?;
    let spelled = typed.dir.join(&typed.name);
    let is_link = std::fs::symlink_metadata(&spelled).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return Ok(typed);
    }
    let target = std::fs::canonicalize(&spelled).map_err(|e| {
        format!(
            "`{}` is a symlink whose target cannot be resolved ({e}); nothing was read or written",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        )
    })?;
    split_typed(&target)
}

/// The spelling's own split: the parent as spelled (`.` for a bare
/// name) and the final component.
fn split_typed(path: &Path) -> Result<LeafAt, String> {
    if path.is_dir() {
        return Err(format!(
            "failed to read {}: is a directory",
            workspace::message_path(path)
        ));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} names no file", workspace::message_path(path)))?
        .to_string();
    if name.is_empty() {
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => path,
        };
        if dir.is_dir() {
            return Err(format!(
                "failed to read {}: is a directory",
                workspace::message_path(path)
            ));
        }
        return Err(format!("{} names no file", workspace::message_path(path)));
    }
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok(LeafAt { dir, name })
}

/// The operator's atomic rewrite (`fmt`, and `fix` in an open
/// universe), at the SAME resolved leaf the read used: the typed leaf
/// was resolved ONCE per
/// invocation — a link to the file it names — and the pair arrives
/// here from the read, so the temp file is created beside THAT file
/// with `openat(parent_fd, O_CREAT | O_EXCL | O_NOFOLLOW)` and moved
/// into place with `renameat`; a leaf that is not a regular file is
/// refused before anything is written, and the typed link, however it
/// points by now, is never consulted again.
///
/// A temp written beside the link and `rename`d over it would
/// **destroy the symlink** — `fmt link.nml` turning a link into a
/// regular file, silently. The write lands where the read came from —
/// the target holds the new text, the link stands — which is what
/// `rustfmt`, `gofmt`, `black` and `prettier` do with a linked file
/// (measured: they format the
/// target in place and leave the link), made atomic and `O_NOFOLLOW`
/// at the resolved leaf. A closed universe never reaches this writer
/// for a linked key: the kernel rejects it before any read (NML2083).
pub(crate) fn write_file_atomically(
    path: &Path,
    at: &LeafAt,
    contents: &str,
) -> Result<(), String> {
    write_leaf(&at.dir, &at.name, contents.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// The write's own advice on a refused link (see [`leaf_advice`]).
#[cfg(unix)]
fn write_advice(e: nml_validate::workspace::OpenError) -> String {
    match e {
        nml_validate::workspace::OpenError::Symlink { .. } => format!(
            "{e} — the leaf became a link between its resolution and the write; nothing was \
             written — run again"
        ),
        e => e.to_string(),
    }
}

/// The handle-anchored write (unix): the kernel's own `write_beneath`,
/// anchored at the leaf's parent — original permission bits preserved
/// by handle, never through a link.
#[cfg(unix)]
fn write_leaf(dir: &Path, leaf: &str, bytes: &[u8]) -> Result<(), String> {
    nml_validate::workspace::write_beneath(dir, &[leaf], bytes).map_err(write_advice)
}

/// Elsewhere (Windows): the classify-then-write shape the read uses on
/// that lane — the leaf is `lstat`ed, a link or a non-regular file
/// refused, then the temp is written and renamed. A same-host racer
/// swapping the leaf between the two is not closed here (E23's boundary
/// stands on that lane, as it does for the read).
#[cfg(not(unix))]
fn write_leaf(dir: &Path, leaf: &str, bytes: &[u8]) -> Result<(), String> {
    let path = dir.join(leaf);
    let mode = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!("`{leaf}` is a symlink (refused before write)"));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(format!(
                "`{leaf}` is not a regular file (refused before write)"
            ));
        }
        Ok(meta) => Some(meta.permissions()),
        Err(_) => None,
    };
    let tmp_path = dir.join(format!(".{leaf}.tmp-{}", std::process::id()));
    std::fs::write(&tmp_path, bytes).map_err(|e| e.to_string())?;
    // Preserve the original's permission bits: the temp file is created at
    // the umask default, and the rename would otherwise silently widen a
    // restricted config (0600 -> 0644). A file being created fresh keeps
    // the default.
    if let Some(mode) = mode {
        std::fs::set_permissions(&tmp_path, mode).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            e.to_string()
        })?;
    }
    std::fs::rename(&tmp_path, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every product verb documents its exit codes in `--help` (clig.dev:
    /// "document exit codes in --help") — an invariant of the one table
    /// the dispatcher reads: every [`VERBS`] entry carries at least one
    /// `exits` row, no verb is registered twice, `help` (a page, not a
    /// verb) is not among them, and the top-level page lists each one.
    /// (A source census used to scan the binary's files for each
    /// fn-local `const SPEC`; the only product Spec with `exits: &[]`
    /// was the hidden composition dump's, gone with the golden.)
    #[test]
    fn every_dispatched_verb_documents_its_exits() {
        let page = usage_text();
        let mut names: Vec<&str> = Vec::new();
        for verb in VERBS {
            let name = verb.spec.verb;
            assert!(
                !verb.spec.exits.is_empty(),
                "`{name}` documents no exit codes"
            );
            assert!(!names.contains(&name), "`{name}` is registered twice");
            assert!(
                page.lines().any(|l| l.trim_start().starts_with(name)),
                "the top-level page does not list `{name}`"
            );
            names.push(name);
        }
        assert!(!names.contains(&"help"), "help is a page, not a verb");
        assert_eq!(verb_names().len(), VERBS.len() + 1);
    }

    /// The top-level page wraps at 80 columns like every verb's page
    /// (`invocation::HELP_WIDTH`): an 80-column terminal or a CI log
    /// viewer shows every row whole.
    #[test]
    fn the_top_level_page_wraps_at_eighty_columns() {
        for line in usage_text().lines() {
            let width = line.chars().count();
            assert!(width <= 80, "{width} columns: {line}");
        }
    }

    /// E35 (arch finding 2/5): the explain hint names the first ERROR's
    /// code, falling back to the first warning's — a warning printed
    /// before the error (an inert-input NML2080 note above an NML2064)
    /// no longer wins the hint.
    #[test]
    fn explain_hint_prefers_the_first_error_over_an_earlier_warning() {
        use nml_core::diagnostic::codes;
        let mut warning = Diagnostic::error("w").with_code(codes::FACET_VIOLATION);
        warning.severity = Severity::Warning;
        let error = Diagnostic::error("e").with_code(codes::SEALED_FIELD_VIOLATION);
        let mut hint = Hint::default();
        hint.note(&warning);
        assert_eq!(hint.code(), Some(codes::FACET_VIOLATION), "a warning alone");
        hint.note(&error);
        assert_eq!(
            hint.code(),
            Some(codes::SEALED_FIELD_VIOLATION),
            "the error wins"
        );
        let mut later = Diagnostic::error("e2").with_code(codes::FACET_VIOLATION);
        later.severity = Severity::Error;
        hint.note(&later);
        assert_eq!(
            hint.code(),
            Some(codes::SEALED_FIELD_VIOLATION),
            "the FIRST error"
        );
        let mut none = Hint::default();
        none.note(&Diagnostic::error("uncoded"));
        assert_eq!(none.code(), None);
    }

    /// `Related.source` rendering (RFC 0019 plan item 2), pinned at the
    /// renderer because both consumers compose single-file today: a
    /// same-file note through the checked map; a foreign note through
    /// ITS OWN file's map — read THROUGH THE UNIVERSE (A3); an
    /// unreadable path (outside the root) without a range — never the
    /// right file with a wrong range.
    #[test]
    fn notes_locate_in_their_own_files() {
        let checked_text = "a = 1\n";
        let map = nml_core::span::SourceMap::new(checked_text);
        let path = Path::new("main.nml");
        let own = "main.nml".to_string();
        let mut foreign = std::collections::HashMap::new();

        let dir = crate::scratch::Scratch::new("note-line");
        let b = dir.join("b.nml");
        std::fs::write(&b, "x = 1\ny = 2\n    z = 3\n").unwrap();
        // A foreign note names its file by KEY (step 0f); the universe
        // turns the key back into a path through the root.
        let b_name = "b.nml".to_string();
        let ws = workspace::Workspace::open(Some(&dir), &[b.display().to_string()]).expect("opens");

        let diag = Diagnostic::error("sealed")
            .with_span(Span::new(0, 1))
            .with_related_in(Span::new(0, 1), "sealed here", None)
            .with_related_in(Span::new(10, 11), "sealed here", Some(b_name.clone()))
            .with_related_in(
                Span::new(3, 4),
                "sealed here",
                Some("no/such/file.nml".into()),
            );

        let lines: Vec<String> = diag
            .related
            .iter()
            .map(|rel| note_line(Some(&ws), path, &map, &own, &mut foreign, &diag, rel))
            .collect();
        assert_eq!(lines[0], "main.nml:1:1: note: sealed here");
        assert_eq!(
            lines[1],
            format!("{b_name}:2:5: note: sealed here"),
            "byte 10 is line 2 col 5 of b.nml, not of the checked file"
        );
        assert_eq!(
            lines[2], "no/such/file.nml: note: sealed here (bytes 3..4)",
            "an unreadable path renders without a wrong range"
        );
        // Without a universe (`parse`/`fmt`) a foreign note is never
        // read by path: it renders without a range.
        let mut none = std::collections::HashMap::new();
        let line = note_line(None, path, &map, &own, &mut none, &diag, &diag.related[1]);
        assert_eq!(line, format!("{b_name}: note: sealed here (bytes 10..11)"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A note in a live manifest the universe holds — here one that FAILED
    /// to load, its text kept — is located by the universe's one derivation
    /// (the row's), never by a second read: a span within the kept text
    /// prints `key:line:col:`, and a span past it prints the byte-span form
    /// (a bare line index over a re-read would clamp a stale offset onto the
    /// last line and print the right file with a wrong range).
    /// A cause locates as a note does — in the row's own file through
    /// its map, in a file the universe cannot read by name alone — and
    /// states no place it cannot map (`line`/`col` null, the file named);
    /// a row that wraps nothing yields no `cause` at all.
    #[test]
    fn a_cause_locates_like_a_note_and_states_no_place_it_cannot_map() {
        use nml_core::diagnostic::codes;
        let map = nml_core::span::SourceMap::new("a\nbb\n");
        let mut none = std::collections::HashMap::new();
        let inner = Diagnostic::error("why")
            .with_code(codes::DUPLICATE_ENTRY)
            .with_span(Span::new(2, 3));
        let wrap = || Diagnostic::error("wrap").with_code(codes::RESOLUTION_INPUT_UNLOADABLE);
        let own = wrap().caused_by(&inner, None);
        assert_eq!(
            cause_value(None, &map, "own.nml", &mut none, &own).expect("a cause"),
            serde_json::json!({
                "code": "NML2093", "source": "own.nml", "line": 2, "col": 1, "message": "why"
            })
        );
        let spanless = wrap().caused_by(
            &Diagnostic::error("why").with_code(codes::DUPLICATE_ENTRY),
            Some("other.nml".to_string()),
        );
        let v = cause_value(None, &map, "own.nml", &mut none, &spanless).expect("a cause");
        assert_eq!(v["source"], "other.nml", "{v}");
        assert!(v["line"].is_null() && v["col"].is_null(), "no place: {v}");
        let unread = wrap().caused_by(&inner, Some("elsewhere.nml".to_string()));
        let v = cause_value(None, &map, "own.nml", &mut none, &unread).expect("a cause");
        assert_eq!(v["source"], "elsewhere.nml", "{v}");
        assert!(v["line"].is_null() && v["col"].is_null(), "unread: {v}");
        assert!(cause_value(None, &map, "own.nml", &mut none, &inner).is_none());
    }

    #[test]
    fn a_manifest_note_locates_through_the_universe_and_a_stale_span_keeps_the_byte_form() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate sits in the repo")
            .join("tests/fixtures/workspace-dup");
        let target = root.join("tenants/cu/plain.flow.nml");
        let ws = workspace::Workspace::open(Some(&root), &[target.display().to_string()])
            .expect("the fixture opens");
        let text = ws
            .discovery()
            .manifest_text("demo.package.nml")
            .expect("a failed manifest's text is kept")
            .to_string();
        let first_files = text.find("files:").expect("the block spelling");
        let stale = text.len() + 5;
        let diag = Diagnostic::error("x")
            .with_source("demo.package.nml".to_string())
            .with_related(Span::new(first_files, first_files + 5), "first")
            .with_related(Span::new(stale, stale + 1), "stale");
        let empty = nml_core::span::SourceMap::new("");
        let mut foreign = std::collections::HashMap::new();
        let path = Path::new("demo.package.nml");
        let lines: Vec<String> = diag
            .related
            .iter()
            .map(|rel| note_line(Some(&ws), path, &empty, "", &mut foreign, &diag, rel))
            .collect();
        assert_eq!(lines[0], "demo.package.nml:11:9: note: first");
        assert_eq!(
            lines[1],
            format!(
                "demo.package.nml: note: stale (bytes {stale}..{})",
                stale + 1
            ),
            "past the kept text: the byte-span form, never a clamped location"
        );
        assert!(
            foreign.is_empty(),
            "no file was read for a manifest the universe holds"
        );
    }

    /// A3's structural pin: every read
    /// of repository content in this file goes through the universe
    /// (`Workspace::read_target` / `read_source`); the workspace-free
    /// verbs' operator input goes through `read_file`, and `read_file`
    /// itself is the kernel's one reader (`read_beneath`) at the leaf
    /// beneath its own parent, so a symlinked leaf, a FIFO, a device and
    /// a directory are all refused at the open and the cap and the UTF-8
    /// rule are the kernel's. There is therefore no by-path read left in
    /// this file at all, no open of its own (the opener is the reader's,
    /// never called here), and exactly one call of the reader, inside
    /// `read_file`. Every needle is spelled by `concat!` so this test is
    /// never its own hit.
    #[test]
    fn the_only_operator_read_in_main_is_read_file_and_it_reads_beneath() {
        let src = include_str!("main.rs");
        let by_path = concat!("std::fs::", "read");
        assert_eq!(
            src.matches(by_path).count(),
            0,
            "a by-path read reappeared in main.rs"
        );
        let opener = concat!("open_", "beneath(");
        assert_eq!(
            src.matches(opener).count(),
            0,
            "an open of its own reappeared in main.rs — the reader opens"
        );
        let reader = concat!("read_", "beneath(");
        let hits: Vec<usize> = src.match_indices(reader).map(|(i, _)| i).collect();
        assert_eq!(hits.len(), 1, "reader calls in main.rs: {}", hits.len());
        let before = &src[..hits[0]];
        let fn_start = before.rfind("\nfn ").expect("inside a fn");
        assert!(
            before[fn_start..].starts_with("\nfn read_file("),
            "the read is not inside read_file"
        );
    }

    #[test]
    fn sanitized_escapes_hostile_paths_and_is_idempotent() {
        // A walked repo filename can carry terminal-escape bytes or bidi
        // overrides; the sanitizer must neutralize both — and escaping
        // twice must not double-escape (escape_default output is
        // escape-free ASCII).
        let hostile = "ev\u{1b}]0;pwned\u{7}il\u{202e}.nml";
        let once = sanitized(hostile);
        assert!(
            !once.contains('\u{1b}') && !once.contains('\u{202e}'),
            "{once}"
        );
        assert!(once.contains("\\u{1b}"), "escaped visibly: {once}");
        assert_eq!(sanitized(&once), once, "idempotent");
    }

    /// `check` and `validate` parse their target ONCE — the
    /// AST, the extracted schema and both finding sets come from one
    /// tree, and the loader takes the extraction in place of the text.
    /// Read at the parse counter (thread-local, structural): a self-
    /// contained file is one parse; with a `--schema` directory of two
    /// files it is one plus those two. Pre-fold: two parses of the
    /// target, the first AST held across the second.
    #[test]
    fn check_and_validate_parse_the_target_exactly_once() {
        use nml_core::cst::parses_on_this_thread;
        let dir = crate::scratch::Scratch::new("one-parse");
        std::fs::create_dir_all(dir.join("schemas")).unwrap();
        let target = dir.join("a.flow.nml");
        std::fs::write(
            &target,
            "model thing:\n    v string\n\nthing T:\n    v = \"x\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("schemas/s1.model.nml"),
            "model other:\n    w string\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("schemas/s2.model.nml"),
            "model more:\n    w string\n",
        )
        .unwrap();
        let ws =
            workspace::Workspace::open(Some(&dir), &[target.display().to_string()]).expect("opens");
        let arg = target.to_string_lossy().into_owned();

        let before = parses_on_this_thread();
        let opts = CheckOpts {
            schema_dir: None,
            strict: false,
        };
        check_one(&ws, &arg, &opts).expect("clean");
        assert_eq!(parses_on_this_thread() - before, 1, "check: one parse");

        let before = parses_on_this_thread();
        let opts = CheckOpts {
            schema_dir: Some(dir.join("schemas")),
            strict: false,
        };
        check_one(&ws, &arg, &opts).expect("clean");
        assert_eq!(
            parses_on_this_thread() - before,
            3,
            "check --schema: the target once, each directory source once"
        );

        let before = parses_on_this_thread();
        validate_one(&ws, &arg).expect("clean");
        assert_eq!(parses_on_this_thread() - before, 1, "validate: one parse");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The operator's atomic rewrite lands at the leaf the read resolved
    /// — `openat(parent, O_CREAT | O_EXCL | O_NOFOLLOW)` + `renameat` —
    /// so a leaf that became a SYMLINK between the read and the write is
    /// refused with the write's own advice and its target is never
    /// written through. (A by-path `std::fs::write` at the resolved leaf
    /// passed every rewrite pin: the repoint pins move the TYPED link,
    /// not the resolved leaf.)
    #[cfg(unix)]
    #[test]
    fn an_open_universe_rewrite_refuses_a_leaf_that_became_a_link() {
        let dir =
            std::env::temp_dir().join(format!("nml-leaf-became-a-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("real.nml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::write(dir.join("victim.nml"), "victim\n").unwrap();
        let at = LeafAt {
            dir: dir.clone(),
            name: "real.nml".to_string(),
        };
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("victim.nml", &path).unwrap();
        let err = write_file_atomically(&path, &at, "new\n")
            .expect_err("a leaf that became a link is refused");
        assert!(
            err.contains(
                "the leaf became a link between its resolution and the write; nothing was written"
            ),
            "{err}"
        );
        // Read through `io::Read` (this file's ratchet admits no by-path
        // `std::fs` read, the product's rule; the test reads its own
        // scratch).
        let mut victim = String::new();
        std::io::Read::read_to_string(
            &mut std::fs::File::open(dir.join("victim.nml")).unwrap(),
            &mut victim,
        )
        .unwrap();
        assert_eq!(victim, "victim\n", "never written through the link");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link stands"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
