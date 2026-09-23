//! The machine-readable output sink: `--json`
//! turns every workspace verb's stdout into line-delimited JSON — one
//! object per line, `"type"`-discriminated — stderr stays silent and
//! exit codes are unchanged, so a pipeline keeps its `set -e` semantics
//! and parses the rows for detail. Human output is untouched when the
//! flag is absent. The shape is documented in
//! `docs/guides/validate-in-ci.md`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufWriter, IsTerminal, LineWriter, Stderr, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use nml_core::diagnostic::{Code, Diagnostic, Severity};
use serde_json::{Value, json};

/// The run's stderr, BUFFERED. Rust's `Stderr` is
/// unbuffered by contract and `eprintln!` issues one `write(2)` PER
/// FORMAT FRAGMENT, so a diagnostic flood paid ~7 syscalls per finding:
/// a 16 MiB file yielding 980,001 `NML2001` errors spent 94 s of 129 s
/// wall in the kernel. Buffering is the whole fix for that term.
///
/// The buffer's SHAPE follows the destination, so nothing a human sees
/// changes: a terminal gets a [`LineWriter`] (every line still appears
/// as it is produced), a pipe, a file or a CI log gets a 256 KiB
/// [`BufWriter`] — which is exactly where the flood is paid. Stdout is
/// flushed through [`flush_err`] before every human line this crate
/// writes, so the two streams interleave at every point they interleave
/// today.
enum ErrSink {
    Terminal(LineWriter<Stderr>),
    Redirected(BufWriter<Stderr>),
}

impl Write for ErrSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Terminal(w) => w.write(buf),
            Self::Redirected(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Terminal(w) => w.flush(),
            Self::Redirected(w) => w.flush(),
        }
    }
}

static ERR: OnceLock<Mutex<ErrSink>> = OnceLock::new();

fn err_sink() -> &'static Mutex<ErrSink> {
    ERR.get_or_init(|| {
        let err = std::io::stderr();
        Mutex::new(if err.is_terminal() {
            ErrSink::Terminal(LineWriter::new(err))
        } else {
            ErrSink::Redirected(BufWriter::with_capacity(256 * 1024, err))
        })
    })
}

/// Run `f` over the buffered stderr. A poisoned lock (a panic while a
/// line was being written) is taken anyway: the alternative is a run
/// that stops reporting.
fn with_err(f: impl FnOnce(&mut ErrSink)) {
    let mut sink = match err_sink().lock() {
        Ok(sink) => sink,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut sink);
}

// ---------------------------------------------------------------------
// The human rendering: Unicode typography, or ASCII outside a UTF-8
// locale (cargo's `term.unicode` model).
// ---------------------------------------------------------------------

/// The tool's own typography and its ASCII spelling — the ONE table:
/// [`fold_ascii`] folds by it, and the source ratchet
/// (`every_glyph_the_sentences_use_is_in_the_fold_table`) refuses a
/// glyph a product sentence adopts without a row here.
pub(crate) const TYPOGRAPHY: &[(char, &str)] = &[
    ('\u{2014}', "--"),  // — em dash
    ('\u{2013}', "-"),   // – en dash
    ('\u{2026}', "..."), // … ellipsis
    ('\u{2039}', "<"),   // ‹
    ('\u{203a}', ">"),   // ›
    ('\u{21d2}', "=>"),  // ⇒
    ('\u{2192}', "->"),  // →
    ('\u{00d7}', "x"),   // ×
    ('\u{2264}', "<="),  // ≤
    ('\u{2265}', ">="),  // ≥
    // The `explain` document's own typography (the error index).
    ('\u{00b7}', "|"),     // · band separator
    ('\u{00b9}', "^1"),    // ¹ superscript
    ('\u{201c}', "\""),    // “
    ('\u{00a7}', "sec. "), // §
    ('\u{2194}', "<->"),   // ↔
    ('\u{00b5}', "u"),     // µ (µs is spelled `us`)
];

/// Whether the HUMAN sinks may render non-ASCII — decided ONCE per run.
/// Cargo's `term.unicode` model: `NML_UNICODE=0|1` overrides
/// (`CARGO_TERM_UNICODE`'s twin); a Windows console decodes Unicode
/// itself; on every other host the codeset of `LC_ALL`, else `LC_CTYPE`,
/// else `LANG` (POSIX precedence — `C`, `POSIX` and an unset locale
/// carry none) decides, as gcc's diagnostics quote by. The JSON stream
/// is UTF-8 whatever the locale; every sentence, pin and transcript
/// keeps its Unicode spelling — only the last byte boundary folds.
static UNICODE: OnceLock<bool> = OnceLock::new();

fn unicode() -> bool {
    *UNICODE.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        unicode_setting(
            var("NML_UNICODE").as_deref(),
            cfg!(windows),
            var("LC_ALL").as_deref(),
            var("LC_CTYPE").as_deref(),
            var("LANG").as_deref(),
        )
    })
}

/// The pure rule behind [`unicode`]: the override first (`0`/`false`/
/// `no`/`off` and `1`/`true`/`yes`/`on`, case-insensitive; anything
/// else is no override), then Windows, then the first NON-EMPTY of the
/// three locale variables — its codeset (the part after `.`, before
/// any `@`) spelled `UTF-8` or `utf8` in any case.
pub(crate) fn unicode_setting(
    override_: Option<&str>,
    windows: bool,
    lc_all: Option<&str>,
    lc_ctype: Option<&str>,
    lang: Option<&str>,
) -> bool {
    if let Some(value) = override_.map(|v| v.trim().to_ascii_lowercase()) {
        match value.as_str() {
            "0" | "false" | "no" | "off" => return false,
            "1" | "true" | "yes" | "on" => return true,
            _ => {}
        }
    }
    if windows {
        return true;
    }
    [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|v| !v.is_empty())
        .is_some_and(codeset_is_utf8)
}

fn codeset_is_utf8(locale: &str) -> bool {
    let Some((_, rest)) = locale.split_once('.') else {
        return false;
    };
    let codeset = rest.split('@').next().unwrap_or("");
    codeset.replace('-', "").eq_ignore_ascii_case("utf8")
}

// ---------------------------------------------------------------------
// Colour: the severity prefixes on a colour-capable stderr, and nowhere
// else — the no-color.org and CLICOLOR contracts, rustc's palette.
// ---------------------------------------------------------------------

/// Whether the human stderr may carry colour — decided ONCE per run.
static COLOR: OnceLock<bool> = OnceLock::new();

fn color() -> bool {
    *COLOR.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        let announced = |name: &str| var(name).is_some_and(|v| !v.is_empty());
        !json()
            && color_setting(
                var("NO_COLOR").as_deref(),
                var("CLICOLOR_FORCE").as_deref(),
                std::io::stderr().is_terminal(),
                var("TERM").as_deref(),
                cfg!(windows),
                announced("WT_SESSION") || announced("TERM_PROGRAM") || announced("ConEmuANSI"),
            )
    })
}

/// The pure rule behind [`color`] — the no-color.org contract as cargo
/// reads it. `NO_COLOR` set and non-empty wins: never colour, whatever
/// else says. Else `CLICOLOR_FORCE` set, non-empty and not `0` forces
/// it — a pipe included (a CI log viewer that renders SGR). Else colour
/// only when stderr is a terminal and `TERM` is not `dumb`. On Windows
/// a terminal alone is not enough: the classic console renders SGR only
/// once a mode flag is set that `std` never sets, so colour needs a host
/// that announces VT (Windows Terminal's `WT_SESSION`, VS Code's
/// `TERM_PROGRAM`, ConEmu's `ConEmuANSI`) or the force.
pub(crate) fn color_setting(
    no_color: Option<&str>,
    force: Option<&str>,
    is_tty: bool,
    term: Option<&str>,
    windows: bool,
    vt_host: bool,
) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if force.is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if !is_tty || term == Some("dumb") {
        return false;
    }
    !windows || vt_host
}

/// The level a prefix is painted as — rustc's palette, bold: error
/// red, warning yellow, note (and an info finding) green, help cyan.
#[derive(Clone, Copy)]
pub(crate) enum Level {
    Error,
    Warning,
    Note,
    Help,
}

impl Level {
    fn sgr(self) -> &'static str {
        match self {
            Self::Error => "1;31",
            Self::Warning => "1;33",
            Self::Note => "1;32",
            Self::Help => "1;36",
        }
    }

    /// A future advisory level (`Severity` is `non_exhaustive`) paints
    /// as a note.
    fn of(severity: &Severity) -> Self {
        match severity {
            Severity::Error => Self::Error,
            Severity::Warning => Self::Warning,
            _ => Self::Note,
        }
    }
}

/// `text` painted as `level` on a colour-capable stderr, itself
/// otherwise. ONLY a line's severity prefix ever comes here —
/// `error[NML2064]`, `warning`, `note:`, `help:`, `error:` — never a
/// path, a message or a payload, so a piped or `NO_COLOR` run is
/// byte-identical to what it was and a coloured one differs by the
/// prefix's SGR wrap alone.
pub(crate) fn paint(level: Level, text: &str) -> Cow<'_, str> {
    if !color() {
        return Cow::Borrowed(text);
    }
    Cow::Owned(format!("\x1b[{}m{text}\x1b[0m", level.sgr()))
}

/// A finding's severity prefix as the human line spells it —
/// `error[NML2064]`, `warning[NML2092]`, `info` — the ONE spelling the
/// located reporter, the locationless reporter and `binding`'s block
/// share; painted for stderr when `painted`, plain for the block on
/// stdout (a verb's answer is never coloured).
pub(crate) fn severity_prefix(diag: &Diagnostic, painted: bool) -> String {
    let code = diag.code.map(|c| format!("[{c}]")).unwrap_or_default();
    let plain = format!("{}{code}", diag.severity);
    if painted {
        paint(Level::of(&diag.severity), &plain).into_owned()
    } else {
        plain
    }
}

/// The human sinks' rendering of a SENTENCE. Under a UTF-8 locale the
/// text is itself; otherwise the tool's typography folds by
/// [`TYPOGRAPHY`] and every other non-ASCII character — content: a
/// walked name, an echoed value — is spelled `\u{XXXX}` (the
/// sanitizer's own escape form; git's `core.quotePath` for paths), so
/// the screen is ASCII and content is never silently respelled.
/// Verbatim payloads (a `fix` diff, [`say_raw`]) never come here.
pub(crate) fn fold_ascii(text: &str) -> Cow<'_, str> {
    fold(text, true)
}

/// The human sinks' rendering of a line that is CONTENT — a block the
/// reader pastes into a file: under the ASCII fold every non-ASCII
/// character is spelled `\u{XXXX}`, the escape the language decodes
/// back to the character, and the typography table never applies (an
/// em dash in a walked name is the name's, not the tool's; folded to
/// `--` it would paste as a different key).
fn fold_content(text: &str) -> Cow<'_, str> {
    fold(text, false)
}

fn fold(text: &str, typography: bool) -> Cow<'_, str> {
    if text.is_ascii() || unicode() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else if let Some((_, ascii)) = TYPOGRAPHY.iter().find(|(c, _)| typography && *c == ch) {
            out.push_str(ascii);
        } else {
            out.extend(ch.escape_unicode());
        }
    }
    Cow::Owned(out)
}

/// One line on the buffered stderr (the `eprintln!` replacement). A
/// write failure is silent: stderr is where failures are reported, so
/// there is nowhere to report its own.
pub fn err(args: std::fmt::Arguments<'_>) {
    with_err(|sink| {
        // Under a UTF-8 locale the arguments are written as they are —
        // no String per line; a flood prints its budget's worth of
        // lines and allocates for none of them.
        let written = if unicode() {
            sink.write_fmt(args)
        } else {
            let text = args.to_string();
            sink.write_all(fold_ascii(&text).as_bytes())
        };
        let _ = written.and_then(|()| sink.write_all(b"\n"));
    });
}

/// One line of CONTENT on the buffered stderr — a line of a block the
/// reader pastes, rendered by [`fold_content`]: exact under the ASCII
/// fold, never respelled by the tool's typography.
pub fn err_content(text: &str) {
    with_err(|sink| {
        let _ = sink
            .write_all(fold_content(text).as_bytes())
            .and_then(|()| sink.write_all(b"\n"));
    });
}

/// Bytes on the buffered stderr (the `eprint!` replacement: the usage
/// text already ends in a newline), rendered for the locale.
pub fn err_str(text: &str) {
    let text = fold_ascii(text);
    with_err(|sink| {
        let _ = sink.write_all(text.as_bytes());
    });
}

/// Push the buffer out. Called before every human line on stdout and at
/// every exit ([`exit`]), so no run can end holding output.
pub fn flush_err() {
    with_err(|sink| {
        let _ = sink.flush();
    });
}

/// The crate's ONE exit (structurally pinned): the stderr buffer is
/// pushed out first. A bare `process::exit` here would drop the run's
/// diagnostics on the floor.
pub fn exit(code: i32) -> ! {
    flush_err();
    std::process::exit(code)
}

/// Push the buffer out when the process PANICS, before the default hook
/// prints the panic: the findings a run reported before
/// it died must not die with the buffer — [`exit`] covers every
/// deliberate exit, this covers the involuntary one. `try_lock`, never
/// `lock`: the panic may have happened while a line was being written
/// (the lock held on this very thread), and a hook that blocked on it
/// would hang the exit; a poisoned lock is taken anyway. The default
/// hook then runs unchanged, so the panic message keeps its shape.
pub fn flush_on_panic() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        match err_sink().try_lock() {
            Ok(mut sink) => {
                let _ = sink.flush();
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let _ = poisoned.into_inner().flush();
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        default(info);
    }));
}

/// A verb's `--help` page: output, not a run (clig.dev — stdout, exit
/// 0), so it leaves HERE with no closing `summary` row even under
/// `--json` (a page followed by a JSON row would be the one path that
/// mixed human text into the stream).
pub fn help_page(text: &str) -> ! {
    say_str(text);
    exit(0)
}

/// A human line on stdout, with the stderr buffer pushed out first so
/// the two streams interleave exactly as they do unbuffered.
pub fn say(args: std::fmt::Arguments<'_>) {
    let mut text = args.to_string();
    text.push('\n');
    say_str(&text)
}

/// Prose on stdout (help pages, the `binding` block, the limits table —
/// each already newline-terminated), rendered for the locale, stderr
/// pushed out first.
pub fn say_str(text: &str) {
    say_raw(&fold_ascii(text))
}

/// Bytes on stdout, VERBATIM — a payload, never prose: the `fix`
/// diff's lines are the file's own text, and a locale must not respell
/// a file. Stderr pushed out first.
pub fn say_raw(text: &str) {
    flush_err();
    let mut out = std::io::stdout().lock();
    if out.write_all(text.as_bytes()).is_err() {
        exit(1)
    }
}

static JSON: AtomicBool = AtomicBool::new(false);
/// The run failed on its INVOCATION: a usage error that propagates as
/// the verb's `Err` — an unknown flag, a missing or surplus positional,
/// a flag without its value, a `--root` that is no directory — closes
/// as an `error` row of kind `usage`, not `run`, and the run exits 2
/// (clig.dev: 2 is the invocation, 1 the domain). The parser and the
/// verbs mark the error where they raise it ([`usage_error`]).
static USAGE: AtomicBool = AtomicBool::new(false);

/// Mark an `Err` as a usage error on its way out (identity on the text).
pub fn usage_error(message: String) -> String {
    USAGE.store(true, Ordering::Relaxed);
    message
}

/// Whether the run's `Err` was marked a usage error: the exit is 2.
pub fn is_usage() -> bool {
    USAGE.load(Ordering::Relaxed)
}

/// `-q`/`--quiet`: errors only — clig.dev's "suppress all non-essential
/// output". A warning or an info is TALLIED (the closing row's counts
/// stay exact) and never printed; the explain hint, the per-file `ok`
/// lines, `fix`'s per-file lines and every closing tally stay silent;
/// errors, a verb's ANSWER (a `binding` block, a diff, an `explain`
/// entry, the `--json` rows), exit codes and the closing row are
/// untouched.
static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet() {
    QUIET.store(true, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}
/// A `result` or `summary` row was emitted: the run's closing verdict
/// (`error: N error(s)`) is then redundant under `--json` and is not
/// emitted as an `error` row (the row already carries the counts).
static VERDICT_EMITTED: AtomicBool = AtomicBool::new(false);

pub fn set_json() {
    JSON.store(true, Ordering::Relaxed);
}

pub fn json() -> bool {
    JSON.load(Ordering::Relaxed)
}

/// Arm JSON mode from the raw argument list BEFORE the verb parses it,
/// so even a usage error under `--json` is an `error` row on stdout
/// (stderr silent), never mixed human text.
pub fn detect(args: &[String]) {
    // `--` ends the flags: a file NAMED `--json` after it must not arm
    // the machine mode.
    if args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json")
    {
        set_json();
    }
}

/// The `--json` contract's own number: moved by a rename, a removal, a
/// type change or a narrowed vocabulary — never by an addition
/// (`docs/stability.md`).
pub const FORMAT_VERSION: u32 = 1;

/// The additions axis within [`FORMAT_VERSION`]: moved by every new
/// key, row type or enumerated value. A consumer that validates
/// strictly (the published schema, every row closed) validates against
/// the schema at exactly this revision — the schema pins it as a
/// `const` on the `contract` row, the stream's FIRST row, so a stricter
/// consumer fails there, on the row whose meaning is the version, never
/// on a finding. Revision 1 was the contract's first shape; each
/// revision since is one CHANGELOG entry naming its additions.
///
/// This constant is the ONE source: the schema's `$defs/revision` const,
/// the generated shape record `docs/json/nml-ndjson-v1.shape.txt` and the
/// CHANGELOG's one entry per revision are held to it by
/// `scripts/docs_test.py` — a schema change without a bump, a bump
/// without one, a missing or a doubled ledger entry each fail the docs
/// gate.
pub const REVISION: u32 = 3;

/// The LIBRARY half of the same contract — the public Rust API of
/// `nml-core`, `nml-validate`, `nml-fmt` and `nml-lsp`, recorded in
/// `docs/api/<crate>.api.txt` with one stamp (`apiVersion A, revision
/// R`) and one CHANGELOG entry per stamp, exactly as the wire above.
///
/// The libraries are consumed BY PATH and BY PINNED REV, never from
/// crates.io, so cargo's own version resolution guards nobody: a
/// removed method, a reshaped enum variant or a new private field lands
/// green here and reaches the consumer days later as a compile error
/// nothing announced. `scripts/api_record.py` regenerates and gates the
/// records (it needs `cargo-public-api`, so it runs in the API CI job
/// and `just gate-api`); the test below is the half that needs no
/// tool, and runs in every `cargo test`: the records exist, they agree
/// on ONE stamp, and the CHANGELOG names it.
///
/// The record is NOT the whole gate, and the measurement says why: a
/// new PRIVATE field on a public struct makes it unconstructible
/// downstream and is INVISIBLE to a public-item record (measured on
/// `ast::Arm` — the record read that break as additive). The classifier
/// `cargo-semver-checks` names it (`constructible_struct_adds_private_field`),
/// which is why `just gate-api` runs both.
#[cfg(test)]
mod api_contract {
    use std::path::PathBuf;

    /// The public API's STAMP — `(apiVersion, revision)` — and the ONE
    /// source of it, as [`REVISION`] is the wire's.
    ///
    /// `revision` + 1 for an ADDITION to the public surface;
    /// `apiVersion` + 1 for a BREAK (an item removed or reshaped, a new
    /// public field on a type that had them, a new private one). The
    /// records under `docs/api/` carry the stamp they were generated at, so
    /// the gate compares THIS number with THEIRS: a surface that moved at
    /// an unchanged stamp fails, and so does a stamp that moved with no
    /// CHANGELOG entry naming what moved. One number for the workspace: the
    /// crates version together (`version.workspace = true`) and one
    /// consumer pins them together.
    ///
    /// It lives HERE, in the gate's own module, because it is a REVIEW
    /// stamp with no runtime reader — unlike [`REVISION`], which the
    /// binary PRINTS on every `contract` row. `scripts/api_record.py`
    /// reads the declaration out of this file by name.
    ///
    /// UNPUBLISHED: a review stamp, not a bound — no `nml limits` row
    pub const API_STAMP: (u32, u32) = (5, 1);

    use nml_validate::test_support::scan::workspace;

    /// Every LIBRARY crate has a record (`nml-cli` is a binary; its
    /// contract is the wire above).
    const RECORDED_CRATES: &[&str] = &["nml-core", "nml-validate", "nml-fmt", "nml-lsp"];

    fn record(crate_name: &str) -> PathBuf {
        workspace()
            .join("docs/api")
            .join(format!("{crate_name}.api.txt"))
    }

    /// The `# apiVersion A, revision R` stamp of a record.
    fn stamp(text: &str) -> Option<(u32, u32)> {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix("# apiVersion ")?;
            let (a, rest) = rest.split_once(", revision ")?;
            Some((a.trim().parse().ok()?, rest.trim().parse().ok()?))
        })
    }

    #[test]
    fn every_library_crate_has_a_record_at_one_stamp_the_changelog_names() {
        let mut stamps = Vec::new();
        for name in RECORDED_CRATES {
            let path = record(name);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "{}: {e} — every library crate carries a public-API record; \
                     `just gate-api` regenerates them",
                    path.display()
                )
            });
            let items = text
                .lines()
                .filter(|l| !l.starts_with("# ") && !l.trim().is_empty())
                .count();
            assert!(items > 0, "{}: the record is empty", path.display());
            stamps.push((
                *name,
                stamp(&text).unwrap_or_else(|| {
                    panic!("{}: no `# apiVersion A, revision R` stamp", path.display())
                }),
            ));
        }
        let (_, first) = stamps[0];
        assert_eq!(
            first, API_STAMP,
            "the records are stamped {first:?}, the ONE source (`out.rs` API_STAMP) says {:?} — \
             regenerate with `NML_UPDATE_GOLDEN=1 python3 scripts/api_record.py` after the \
             stamp and its CHANGELOG entry moved",
            API_STAMP
        );
        for (name, s) in &stamps {
            assert_eq!(
                *s, first,
                "{name}: the records carry different stamps — the crates version \
                 together (`version.workspace = true`) and one consumer pins them \
                 together, so there is ONE stamp"
            );
        }
        // The ledger: the stamp moves WITH the sentence that says what
        // moved, as a wire revision does.
        let changelog =
            std::fs::read_to_string(workspace().join("CHANGELOG.md")).expect("CHANGELOG.md reads");
        let entry = format!(
            "- **public API apiVersion {}, revision {}**",
            first.0, first.1
        );
        assert_eq!(
            changelog.matches(&entry).count(),
            1,
            "CHANGELOG.md must name `{entry}` exactly once — a stamp without a ledger \
             entry is a change nobody wrote down, and a doubled one is two changes \
             sharing a number"
        );
    }
}

/// Stamp the contract's three facts — the format, its revision, the
/// binary that wrote the stream — onto a row: the opening `contract`
/// row and the closing `summary` carry the same three, so a consumer
/// holding only a stream's head or only its tail knows what it reads.
fn stamp_contract(row: &mut Value) {
    if let Some(map) = row.as_object_mut() {
        map.insert("formatVersion".to_string(), json!(FORMAT_VERSION));
        map.insert("revision".to_string(), json!(REVISION));
        map.insert("nmlVersion".to_string(), json!(env!("CARGO_PKG_VERSION")));
    }
}

/// The stream's FIRST row, on every path that emits any row at all — a
/// finding, an answer, a usage error, the closing row: the stream
/// describes itself before it says anything else (clig.dev: versioned,
/// deterministic output for scripts and agents; Terraform's
/// `format_version`, SARIF's `version` — a header the consumer reads
/// before the first result). A `--help` page emits no row and so no
/// header: it is output, not a run.
fn contract_row() -> Value {
    let mut row = json!({"type": "contract"});
    stamp_contract(&mut row);
    row
}

/// Whether the contract row has opened the stream — armed by the first
/// [`emit`], whatever row it carries, so no verb and no exit path can
/// forget the header or emit it twice.
static CONTRACT_STATED: AtomicBool = AtomicBool::new(false);

/// One NDJSON line on stdout — after the stream's `contract` row, written
/// once, ahead of whatever row comes first.
pub fn emit(v: &Value) {
    if !CONTRACT_STATED.swap(true, Ordering::Relaxed) {
        write_row(&contract_row());
    }
    write_row(v);
}

/// The one writer (serde_json's Display is compact: no embedded
/// newlines, so one value is exactly one line).
fn write_row(v: &Value) {
    // Never `println!`: a consumer that closes the pipe early (`| head`)
    // made it PANIC with a human backtrace on stderr, exit 101 — the one
    // thing this contract forbids. A write failure ends the run:
    // nothing more can be said on stdout, stderr stays silent, exit 1 —
    // the contract row's own write included.
    flush_err();
    let mut out = std::io::stdout().lock();
    let line = wire_escaped(&v.to_string());
    if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
        exit(1)
    }
}

/// The one-object-per-LINE contract, kept on the wire: serde
/// escapes only `"`, `\` and U+0000–U+001F, so a walked file name
/// carrying U+0085 (NEL), U+2028/U+2029, a bidi override, U+FEFF or a
/// C1 control rode a row RAW — and a consumer that splits on Unicode
/// line breaks (Python's `str.splitlines()`, most log viewers) saw one
/// row as two unparseable halves. Every character the human renderer
/// escapes (`needs_escape`) is spelled `\uXXXX` here instead; the JSON
/// VALUE is unchanged (a JSON `\u2028` decodes to the same character),
/// only its spelling is. Such characters occur only inside strings in
/// compact serde output, so the rewrite is over the whole line.
fn wire_escaped(line: &str) -> String {
    if !line.chars().any(nml_core::diagnostic::needs_escape) {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 16);
    for ch in line.chars() {
        if nml_core::diagnostic::needs_escape(ch) {
            for unit in ch.encode_utf16(&mut [0u16; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// A row that already states the run's outcome (`result`, `summary`).
pub fn emit_verdict(v: &Value) {
    mark_verdict();
    emit(v);
}

/// The run's verdict is already stated (a `result` row, or the closing
/// `summary` row's own fields): the classic `error: N error(s)` line is
/// then redundant under `--json`.
pub fn mark_verdict() {
    VERDICT_EMITTED.store(true, Ordering::Relaxed);
}

/// The diagnostic row: `{type, source, line, col, severity, code,
/// message, related[], suggestions[]}` and, on a row that reports
/// another finding's refusal, `cause` (`{code, source, line, col,
/// message}` — the key rides only where there is one, so a consumer
/// reads `cause?.code ?? code`). `line`/`col` are null for a
/// locationless finding (a universe note, a kernel rejection). `message`
/// is the sanitized spelling (`rendered_message`), never raw bytes;
/// `suggestions` are the finding's machine-applicable edits, resolved
/// against the files as this run read them (empty for a row that
/// carries none).
pub fn diag_value(
    source: &str,
    loc: Option<(usize, usize)>,
    diag: &Diagnostic,
    related: Vec<Value>,
    suggestions: Vec<Value>,
    cause: Option<Value>,
) -> Value {
    let mut row = json!({
        "type": "diagnostic",
        "source": source,
        "line": loc.map(|l| l.0),
        "col": loc.map(|l| l.1),
        "severity": diag.severity.to_string(),
        "code": diag.code.map(|c| c.to_string()),
        "message": diag.rendered_message(),
        "related": related,
        "suggestions": suggestions,
    });
    if let (Some(map), Some(cause)) = (row.as_object_mut(), cause) {
        map.insert("cause".to_string(), cause);
    }
    row
}

/// A note the fixer emits about its own work (a refusal, a budget
/// stop): an info-severity row with no code.
pub fn info_value(source: &str, loc: Option<(usize, usize)>, message: &str) -> Value {
    json!({
        "type": "diagnostic",
        "source": source,
        "line": loc.map(|l| l.0),
        "col": loc.map(|l| l.1),
        "severity": "info",
        "code": null,
        "message": message,
        "related": [],
        "suggestions": [],
    })
}

/// A fatal or per-target error line: JSON `{type:"error", kind, message,
/// exit}` on stdout under `--json`, else the classic `error: …` on
/// stderr (sanitized — error strings embed walked filesystem names).
/// `kind` says what the row is, so a consumer no longer parses the
/// sentence to tell them apart: `usage` (the invocation itself — a bad
/// flag, a missing target, the `--schema` conflict), `target` (one
/// target of a many-target run failed; the run continued), `run` (the
/// run's closing verdict, when no `result`/`summary` row carries it).
pub fn error_line(message: &str, exit: i32, kind: &str) {
    if json() {
        emit(&json!({"type": "error", "kind": kind, "message": message, "exit": exit}));
    } else {
        err(format_args!(
            "{} {}",
            paint(Level::Error, "error:"),
            crate::sanitized(message)
        ));
    }
}

/// The run's closing verdict (`main`'s `error: …` line): under `--json`
/// it is dropped when a `result`/`summary` row already carries it.
pub fn closing_error(message: &str, exit: i32) {
    if json() && VERDICT_EMITTED.load(Ordering::Relaxed) {
        return;
    }
    let kind = if USAGE.load(Ordering::Relaxed) {
        "usage"
    } else {
        "run"
    };
    error_line(message, exit, kind);
}

// ---------------------------------------------------------------------
// The run's tally: the ONE closing `summary` row, the run-scoped
// universe facts and the per-code finding budget.
// ---------------------------------------------------------------------

/// The default ceiling on findings PRINTED in one run. A cap on by
/// default is the state of the art for a compiler-shaped tool: clang
/// ships `-ferror-limit=20`, and one 16 MiB file here produced 980,001
/// `NML2001` lines — 89 MB of stderr no human and no CI log reads. The
/// COUNTS stay exact whatever the cap: only the printing is bounded, and
/// the run says exactly how many it withheld and under which codes.
///
/// LIMIT: reach=content guards=output surface=cli shown="512" — findings PRINTED per run (`--max-findings`); the counts stay exact
pub const MAX_SHOWN: usize = 512;

/// Per-code fairness: no single code may take more than an eighth of the
/// budget while another code has findings waiting. Without it the first
/// flooding code crowds out every rarer — and usually more important —
/// finding behind it.
fn per_code_cap(total: usize) -> usize {
    (total / 8).max(1)
}

/// The universe the run resolved in, as the closing row states it,
/// including the budget units the walk stopped inside (the A16
/// amendment): a unit's exhaustion leaves the universe
/// CLOSED and its manifests counted — the claim that induced the unit
/// stands above it — so `closed`/`manifests` alone would read as a
/// whole universe. `truncated_units` says which subtrees were denied.
/// The root as the closing row and the `binding` row state it: the
/// path, HOW it was fixed (`RootOrigin::tag`), the fence entry's kind
/// when derived within a VCS fence (`dir`, `file`, `symlink`, `other`)
/// and the entry above the fence that shadows the derived universe —
/// another `.git`, or a root marker above a directory fence — the
/// shapes a planted or submodule `.git` and a nested checkout make,
/// additive fields at `formatVersion: 1`.
#[derive(Clone)]
pub struct RootFacts {
    pub path: String,
    pub origin: &'static str,
    pub fence: Option<&'static str>,
    pub shadowed: Option<String>,
}

impl RootFacts {
    /// The `root` object of a row.
    pub fn row(&self) -> Value {
        json!({
            "path": self.path,
            "origin": self.origin,
            "fence": self.fence,
            "shadowed": self.shadowed,
        })
    }
}

#[derive(Clone)]
struct UniverseFacts {
    /// Whether the universe DECIDES ([`nml_validate::workspace::UniverseState`]):
    /// the kernel owns the two words, and this row renders them — it
    /// spelled `"closed"`/`"open"` itself, in two places, beside a
    /// `closure` field whose three words are a different axis entirely.
    state: nml_validate::workspace::UniverseState,
    /// The kernel's `Closure`, spelled for the row: `complete`,
    /// `truncated` (the whole universe) or `unloadable` (a live input
    /// failed to load) — `closed` alone conflated the three.
    closure: &'static str,
    manifests: usize,
    /// `(unit, stop, why)` — the keys and the bound that was spent
    /// (`entries`, `liveInputBytes`, `unreadable`), in walk order.
    truncated_units: Vec<(String, String, &'static str)>,
    /// `(key, why, entry)` — what the walk left out of its enumeration
    /// by policy (`symlink`, `fifo`, `dotDirectory`, `dotFile`,
    /// `policyDirectory`, `unkeyableName`, `componentBound`), by depth
    /// then key: a gate's
    /// consumer sees the content the run never judged. `entry` is the
    /// entry's own name where the key cannot carry it (`unkeyableName`:
    /// the key is then the holding directory's).
    skipped: Vec<(String, &'static str, Option<String>)>,
}

/// The run's counters. `seen` is EXACT — every finding the run derived,
/// whether or not it was printed — keyed by code (`None` = uncoded).
struct Tally {
    targets: usize,
    errors: usize,
    warnings: usize,
    seen: HashMap<Option<Code>, usize>,
    shown: HashMap<Option<Code>, usize>,
    /// Codes seen whose printed count is still below their fair share.
    /// Maintained incrementally so the fairness rule is O(1) per
    /// finding — a flood of a million findings must not pay a scan of
    /// the code table a million times.
    starved: usize,
    shown_total: usize,
    /// `--max-findings`: `usize::MAX` = uncapped.
    budget: usize,
    root: Option<RootFacts>,
    universe: Option<UniverseFacts>,
    /// How many schema sources a `--schema <dir>` invocation contributed;
    /// `None` when the run was given no `--schema`. `Some(0)` is the fact
    /// worth having: the directory listed fine and held nothing the loader
    /// admits, so the run validated every target against its own
    /// definitions alone (RFC 0026 decision 5).
    schema_sources: Option<usize>,
}

static WITHHELD_REPORTED: AtomicBool = AtomicBool::new(false);

/// The verb's own fields on the closing row (`fix`'s edit counts), set
/// by the verb before it returns so the row keeps ONE shape.
static EXTRA: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

/// Record the verb-specific half of the closing `summary` row.
pub fn set_summary_extra(fields: Vec<(String, Value)>) {
    let mut extra = match EXTRA.lock() {
        Ok(extra) => extra,
        Err(poisoned) => poisoned.into_inner(),
    };
    *extra = fields;
}

static TALLY: OnceLock<Mutex<Tally>> = OnceLock::new();

fn with_tally<T>(f: impl FnOnce(&mut Tally) -> T) -> T {
    let tally = TALLY.get_or_init(|| {
        Mutex::new(Tally {
            targets: 0,
            errors: 0,
            warnings: 0,
            seen: HashMap::new(),
            shown: HashMap::new(),
            starved: 0,
            shown_total: 0,
            budget: MAX_SHOWN,
            root: None,
            universe: None,
            schema_sources: None,
        })
    });
    let mut tally = match tally.lock() {
        Ok(tally) => tally,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut tally)
}

/// `--max-findings <n>`; `0` lifts the cap entirely.
pub fn set_budget(n: usize) {
    with_tally(|t| t.budget = if n == 0 { usize::MAX } else { n });
}

pub fn set_targets(n: usize) {
    with_tally(|t| t.targets = n);
}

/// How many schema sources the invocation's `--schema <dir>` contributed,
/// recorded once before any target — `0` is the disclosure a source-less
/// directory earns (RFC 0026 decision 5). A run with no `--schema` never
/// calls this and its closing row carries `null`.
pub fn set_schema_sources(n: usize) {
    with_tally(|t| t.schema_sources = Some(n));
}

/// The universe the run resolved in, recorded ONCE (claim 4): a
/// run-scoped fact belongs on the run's row, not repeated on every
/// target's. `truncated_units` are the `(unit, stop, why)` rows of the
/// budget units the walk stopped inside — empty when none was;
/// `closure` is the kernel's word on the whole universe.
/// The root the run fixed, recorded FIRST — before any argument is
/// classified and before the walk — so a run refused at its invocation
/// (a target outside the root) still names the universe it was refused
/// against on its closing row, with no universe facts (none was built).
pub fn set_root(root: RootFacts) {
    with_tally(|t| t.root = Some(root));
}

pub fn set_universe(
    state: nml_validate::workspace::UniverseState,
    closure: &'static str,
    manifests: usize,
    truncated_units: Vec<(String, String, &'static str)>,
    skipped: Vec<(String, &'static str, Option<String>)>,
) {
    with_tally(|t| {
        t.universe = Some(UniverseFacts {
            state,
            closure,
            manifests,
            truncated_units,
            skipped,
        });
    });
}

/// Count a finding's severity into the run's exact tally (`errors`,
/// `warnings` on the closing row).
fn count(t: &mut Tally, diag: &Diagnostic) {
    match diag.severity {
        nml_core::diagnostic::Severity::Error => t.errors += 1,
        nml_core::diagnostic::Severity::Warning => t.warnings += 1,
        _ => {}
    }
}

/// Tally a finding WITHOUT deciding whether it prints: `binding`
/// renders its notes in its own block, so the closing row's `errors`
/// and `warnings` learn about them here (a `binding --json` that said
/// `errors: 0` while exiting 1 on a universe error would lie).
pub fn tally(diag: &Diagnostic) {
    with_tally(|t| count(t, diag));
}

/// The error-severity findings this run has reported so far — what the
/// closing row's `errors` will say; a verb's exit follows it.
pub fn errors_reported() -> usize {
    with_tally(|t| t.errors)
}

/// Tally a finding and answer whether it is PRINTED. Both reporters ask
/// here before writing a byte, so the budget and the exact counts have
/// exactly one owner.
///
/// **Fairness** (claim 5c). A finding is printed unless one of three
/// rules refuses it:
///
/// 1. the total budget is spent;
/// 2. this code is past its fair share ([`per_code_cap`]) and what is
///    left of the budget could not still give every STARVED code (seen,
///    printed below its share) its share AND keep one share for a code
///    not yet seen — that last slice is RESERVED, because a rarer, more
///    important finding usually arrives late in a file that a common one
///    floods from line 6. Without the reserve a million `NML2001`s spend
///    the budget before the one `NML2008` at the end of the file is ever
///    derived.
///
/// So a single flooding code prints `budget - share` of the budget and
/// leaves `share` for whatever comes later; two competing codes split
/// it; and the counts stay exact whatever is printed. (A rule that
/// refused a past-share code whenever ANY other code was starved would
/// let a rare code seen FIRST — starved for the rest of the run — cap a
/// later flood at its 64-line share instead of 448.)
pub fn admit(diag: &Diagnostic) -> bool {
    with_tally(|t| {
        count(t, diag);
        // `--quiet`: a warning or an info is counted and never printed —
        // outside the budget, so the withheld trailer names only what
        // the BUDGET withheld.
        if quiet() && !matches!(diag.severity, nml_core::diagnostic::Severity::Error) {
            return false;
        }
        let share = per_code_cap(t.budget);
        let mut first_sight = false;
        let seen = t.seen.entry(diag.code).or_insert_with(|| {
            first_sight = true;
            0
        });
        *seen += 1;
        if first_sight {
            t.starved += 1;
        }
        if t.shown_total >= t.budget {
            return false;
        }
        let mine = t.shown.get(&diag.code).copied().unwrap_or(0);
        if mine >= share
            && t.shown_total
                .saturating_add(t.starved.saturating_add(1).saturating_mul(share))
                >= t.budget
        {
            return false;
        }
        let slot = t.shown.entry(diag.code).or_default();
        *slot += 1;
        if *slot == share {
            t.starved -= 1;
        }
        t.shown_total += 1;
        true
    })
}
/// What the budget withheld: `(hidden total, hidden per code)`, exact,
/// largest first then by code so the trailer and the JSON row are
/// deterministic.
fn withheld() -> (usize, Vec<(String, usize)>) {
    with_tally(|t| {
        let mut per: Vec<(String, usize)> = t
            .seen
            .iter()
            .map(|(code, seen)| {
                let name = code.map_or_else(|| "(uncoded)".to_string(), |c| c.to_string());
                (name, seen - t.shown.get(code).copied().unwrap_or(0))
            })
            .filter(|(_, hidden)| *hidden > 0)
            .collect();
        per.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        (per.iter().map(|(_, n)| n).sum(), per)
    })
}

/// The human trailer, printed once when the budget withheld anything.
/// It names the exact count, the codes behind it (largest first) and the
/// flag that lifts the cap — a truncation a consumer cannot see is the
/// one thing a bounded reporter must never do.
pub fn report_withheld() {
    if WITHHELD_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let (hidden, per) = withheld();
    if hidden == 0 || json() {
        return;
    }
    let listed: Vec<String> = per
        .iter()
        .take(5)
        .map(|(code, n)| format!("{code} \u{00d7}{n}"))
        .collect();
    let more = if per.len() > 5 {
        format!(", and {} more code(s)", per.len() - 5)
    } else {
        String::new()
    };
    let budget = with_tally(|t| t.budget);
    err(format_args!(
        "{} {hidden} more finding(s) not shown (limit {budget}; {}{more}) — pass \
         --max-findings 0 to print them all",
        paint(Level::Note, "note:"),
        listed.join(", ")
    ));
}

/// The run's ONE closing row: `{type:"summary", formatVersion, revision,
/// nmlVersion, verb, exit, targets, errors, warnings, root, universe,
/// closure, manifests, truncatedUnits, withheld}`, emitted LAST by every verb on EVERY
/// path — a clean run, a failing run, a parse error, a usage error. A
/// consumer reads `exit` instead of re-encoding each verb's 0/1/2
/// mapping, and reads the run-scoped universe facts here instead of
/// running `nml binding` to learn them. `closure` is the kernel's
/// word on the whole universe — `complete`, `truncated`, `unloadable` —
/// and `truncatedUnits` lists the budget units the walk stopped inside
/// (`[{unit, stop, why}]`, empty when none; null without a universe) —
/// distinct from `withheld`, which is the finding-PRINTING budget
/// (`{shown, hidden, byCode}`, null when nothing was withheld); `skipped`
/// (`{byWhy, rows, shown, hidden}`, null without a universe) is what the
/// walk left out by policy — exact counts, rows under the same budget.
/// `extra` carries the verb's own fields (`fix`'s edit counts) in the
/// same row, so there is exactly one terminal row type. A `--help` page
/// is output, not a run: it leaves through [`help_page`] with no row.
pub fn emit_summary(verb: &str, exit: i32) {
    if !json() {
        return;
    }
    let (hidden, per) = withheld();
    let (shown, targets, errors, warnings) =
        with_tally(|t| (t.shown_total, t.targets, t.errors, t.warnings));
    let withheld = if hidden == 0 {
        Value::Null
    } else {
        let by_code: serde_json::Map<String, Value> = per
            .iter()
            .map(|(code, hidden)| (code.clone(), json!(hidden)))
            .collect();
        json!({"shown": shown, "hidden": hidden, "byCode": by_code})
    };
    let (root, universe, schema_sources) =
        with_tally(|t| (t.root.clone(), t.universe.clone(), t.schema_sources));
    let truncated_units = universe.as_ref().map(|u| {
        u.truncated_units
            .iter()
            .map(|(unit, stop, why)| json!({"unit": unit, "stop": stop, "why": why}))
            .collect::<Vec<Value>>()
    });
    // The walk's skip report under the same discipline as the findings:
    // the counts by `why` are EXACT over everything the walk left out,
    // the rows are the first `budget` in walk order (depth, then key —
    // the shallowest first), and `hidden` says how many rows the budget
    // held back (`--max-findings 0` lifts it). A tree with thousands of
    // committed links no longer grows its closing row by thousands of
    // rows.
    let budget = with_tally(|t| t.budget);
    let skipped = universe.as_ref().map(|u| {
        let mut by_why: serde_json::Map<String, Value> = serde_json::Map::new();
        for (_, why, _) in &u.skipped {
            let n = by_why.get(*why).and_then(Value::as_u64).unwrap_or(0);
            by_why.insert((*why).to_string(), json!(n + 1));
        }
        let rows: Vec<Value> = u
            .skipped
            .iter()
            .take(budget)
            .map(|(key, why, entry)| match entry {
                Some(entry) => json!({"key": key, "why": why, "entry": entry}),
                None => json!({"key": key, "why": why}),
            })
            .collect();
        let shown = rows.len();
        json!({
            "byWhy": by_why,
            "rows": rows,
            "shown": shown,
            "hidden": u.skipped.len() - shown,
        })
    });
    // The contract's three facts ride the closing row as they ride the
    // opening one (`stamp_contract`): a consumer reading a saved log's
    // tail alone still knows what it reads.
    let mut row = json!({
        "type": "summary",
        "verb": verb,
        "exit": exit,
        "targets": targets,
        "errors": errors,
        "warnings": warnings,
        "root": root.map(|r| r.row()),
        "universe": universe.as_ref().map(|u| u.state.label()),
        "closure": universe.as_ref().map(|u| u.closure),
        "manifests": universe.as_ref().map(|u| u.manifests),
        "truncatedUnits": truncated_units,
        "skipped": skipped,
        "withheld": withheld,
        "schemaSources": schema_sources,
    });
    stamp_contract(&mut row);
    let extra = match EXTRA.lock() {
        Ok(extra) => extra,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(map) = row.as_object_mut() {
        for (key, value) in extra.iter() {
            map.insert(key.clone(), value.clone());
        }
    }
    emit(&row);
}

#[cfg(test)]
mod tests {
    /// The one rule, executed: the override wins in both directions and
    /// only in its vocabulary; Windows is Unicode; else the first
    /// NON-EMPTY locale variable's codeset decides, `-`-blind and
    /// case-blind, `@modifier` ignored; `C`, `POSIX`, empty and unset
    /// are ASCII.
    #[test]
    fn the_unicode_rule_is_cargos() {
        use super::unicode_setting as u;
        assert!(!u(None, false, Some("C"), None, None));
        assert!(!u(None, false, None, Some("POSIX"), None));
        assert!(!u(None, false, None, None, None));
        assert!(!u(None, false, Some(""), Some(""), Some("")));
        assert!(u(None, false, Some("C.UTF-8"), None, None));
        assert!(u(None, false, None, None, Some("en_US.utf8")));
        assert!(u(None, false, None, Some("de_DE.UTF-8@euro"), Some("C")));
        // LC_ALL outranks LC_CTYPE outranks LANG; an EMPTY one is unset.
        assert!(!u(None, false, Some("C"), Some("en_US.UTF-8"), None));
        assert!(u(None, false, Some(""), Some("en_US.UTF-8"), Some("C")));
        assert!(!u(None, false, None, Some("C"), Some("en_US.UTF-8")));
        assert!(!u(None, false, Some("en_US"), None, None));
        assert!(u(None, true, Some("C"), None, None));
        assert!(u(Some("1"), false, Some("C"), None, None));
        assert!(u(Some(" TRUE "), false, None, None, None));
        assert!(!u(Some("0"), true, Some("C.UTF-8"), None, None));
        assert!(!u(Some("off"), false, Some("C.UTF-8"), None, None));
        // Not in the vocabulary: no override.
        assert!(!u(Some("maybe"), false, Some("C"), None, None));
        assert!(u(Some("maybe"), false, Some("C.UTF-8"), None, None));
    }

    /// SOURCE RATCHET: the CLI spells no indentation unit of its own. A
    /// block it prints at the canonical step reads
    /// `nml_core::cst::INDENT_UNIT` — the one constant the formatter, the
    /// insertion engine, the snippet decoder, the on-type handler and this
    /// binary share — so no product string literal under `nml-cli/src`
    /// OPENS with the unit's spelling (the unresolved remedy block's
    /// margin did: a fifth spelling of the unit). A file whose four-space
    /// literals are TYPOGRAPHY of the CLI's own — not a block of the
    /// language — is listed here with its reason, as the glyph ratchet
    /// lists its content files.
    #[test]
    fn the_cli_spells_no_indentation_unit_of_its_own() {
        use nml_validate::test_support::scan::{product_literals, sources, workspace};
        const TYPOGRAPHY_FILES: &[(&str, &str)] = &[(
            "nml-cli/src/invocation.rs",
            "the `--help` page's column gutter (a four-space margin before a 20-wide column)",
        )];
        let workspace = workspace();
        let mut files = Vec::new();
        sources(&workspace.join("nml-cli").join("src"), false, &mut files);
        assert!(
            files.len() >= 5,
            "the scanner found only {} sources",
            files.len()
        );
        let mut offenders = Vec::new();
        let mut exempt_seen = 0usize;
        for file in &files {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if TYPOGRAPHY_FILES.iter().any(|(f, _)| *f == rel) {
                exempt_seen += 1;
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for literal in product_literals(&text) {
                if literal.starts_with(nml_core::cst::INDENT_UNIT) {
                    offenders.push(format!("{}: {literal:?}", file.display()));
                }
            }
        }
        assert_eq!(
            exempt_seen,
            TYPOGRAPHY_FILES.len(),
            "every listed typography file must exist (a stale exemption is a lie)"
        );
        assert!(
            offenders.is_empty(),
            "a product literal opens with the indentation unit's spelling — read \
             `nml_core::cst::INDENT_UNIT` instead:\n{}",
            offenders.join("\n")
        );
    }

    /// SOURCE RATCHET: every non-ASCII character a product string
    /// literal carries — the sentences of every crate the CLI prints
    /// for, OUTSIDE their `#[cfg(test)] mod` blocks wherever those sit
    /// (`test_support::scan`, the one lexer: a ratchet that cut a file
    /// at its first `#[cfg(test)]` never read `main.rs` past line 18) —
    /// and every non-ASCII character of the documents the CLI prints
    /// (`explain`'s error index) is either the tool's typography (a row
    /// of `TYPOGRAPHY`) or lives in a file whose non-ASCII is CONTENT
    /// vocabulary, listed here with its reason. A sentence or a document
    /// that adopts a new glyph fails this test until the fold table can
    /// spell it.
    #[test]
    fn every_glyph_the_sentences_use_is_in_the_fold_table() {
        use nml_validate::test_support::scan::{product_literals, sources, workspace};
        use std::collections::BTreeMap;
        // Non-ASCII that is content vocabulary, not typography.
        const CONTENT_FILES: &[(&str, &str)] = &[(
            "crates/nml-core/src/source_policy.rs",
            "the C1-to-Windows-1252 table names the glyph a stray byte was meant to be",
        )];
        // The documents the CLI prints whole (`explain`).
        const DOCUMENTS: &[&str] = &["crates/nml-core/assets/error-index.md"];
        let workspace = workspace();
        let mut files = Vec::new();
        sources(&workspace.join("crates"), false, &mut files);
        sources(&workspace.join("nml-cli").join("src"), false, &mut files);
        files.sort();
        assert!(
            files.len() > 40,
            "the scanner found only {} sources",
            files.len()
        );
        let mut foreign: BTreeMap<char, Vec<String>> = BTreeMap::new();
        let mut seen_typography = 0usize;
        let mut scanned_main = false;
        let mut texts: Vec<(String, Vec<String>)> = Vec::new();
        for file in &files {
            let rel = file
                .strip_prefix(&workspace)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if CONTENT_FILES.iter().any(|(f, _)| *f == rel) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            let literals = product_literals(&text);
            // The blind spot, pinned: main.rs declares a test module at
            // its top and its usage text (line 400+) carries an em dash.
            if rel == "nml-cli/src/main.rs" {
                scanned_main = literals.iter().any(|l| l.contains('\u{2014}'));
            }
            texts.push((rel, literals));
        }
        assert!(
            scanned_main,
            "main.rs was not scanned past its first #[cfg(test)]"
        );
        for doc in DOCUMENTS {
            let text = std::fs::read_to_string(workspace.join(doc)).expect(doc);
            texts.push(((*doc).to_string(), vec![text]));
        }
        for (rel, literals) in &texts {
            for lit in literals {
                for ch in lit.chars().filter(|c| !c.is_ascii()) {
                    if super::TYPOGRAPHY.iter().any(|(c, _)| *c == ch) {
                        seen_typography += 1;
                    } else {
                        foreign.entry(ch).or_default().push(rel.clone());
                    }
                }
            }
        }
        assert!(
            seen_typography > 50,
            "the literal scanner is broken: {seen_typography} glyphs"
        );
        for v in foreign.values_mut() {
            v.sort();
            v.dedup();
        }
        assert!(
            foreign.is_empty(),
            "a product sentence uses a glyph the fold table cannot spell — add a row to \
             `out::TYPOGRAPHY` (or list the file under CONTENT_FILES with a reason): {foreign:#?}"
        );
        for (file, why) in CONTENT_FILES {
            assert!(
                !why.is_empty() && workspace.join(file).exists(),
                "{file}: {why}"
            );
        }
    }

    /// The colour rule, executed: `NO_COLOR` (non-empty) wins over
    /// everything, `CLICOLOR_FORCE` (non-empty, not `0`) wins over a
    /// pipe and a dumb terminal, a terminal colours unless `TERM=dumb`,
    /// a pipe never does, and a Windows terminal only when a VT host
    /// announces itself.
    #[test]
    fn the_colour_rule_is_no_color_orgs() {
        use super::color_setting as c;
        assert!(c(None, None, true, Some("xterm-256color"), false, false));
        assert!(c(None, None, true, None, false, false));
        assert!(!c(None, None, false, Some("xterm"), false, false), "a pipe");
        assert!(
            !c(None, None, true, Some("dumb"), false, false),
            "TERM=dumb"
        );
        assert!(
            !c(Some("1"), None, true, Some("xterm"), false, false),
            "NO_COLOR"
        );
        assert!(
            !c(Some("x"), Some("1"), true, None, false, false),
            "NO_COLOR beats the force"
        );
        assert!(
            c(Some(""), None, true, None, false, false),
            "an EMPTY NO_COLOR is unset"
        );
        assert!(
            c(None, Some("1"), false, Some("dumb"), false, false),
            "the force beats a pipe and dumb"
        );
        assert!(
            !c(None, Some("0"), false, None, false, false),
            "CLICOLOR_FORCE=0 is no force"
        );
        assert!(
            !c(None, Some(""), false, None, false, false),
            "an empty force is no force"
        );
        assert!(
            !c(None, None, true, None, true, false),
            "a Windows console with no VT host"
        );
        assert!(
            c(None, None, true, None, true, true),
            "a VT host on Windows"
        );
        assert!(
            c(None, Some("1"), false, None, true, false),
            "the force on Windows"
        );
    }

    /// The fold is total over the table and lossless past it.
    #[test]
    fn the_fold_spells_typography_and_escapes_content() {
        for (ch, ascii) in super::TYPOGRAPHY {
            assert!(ascii.is_ascii() && !ascii.is_empty(), "{ch:?}");
        }
        let folded: String = "a — b … ‹c› é"
            .chars()
            .map(
                |ch| match super::TYPOGRAPHY.iter().find(|(c, _)| *c == ch) {
                    Some((_, a)) => a.to_string(),
                    None if ch.is_ascii() => ch.to_string(),
                    None => ch.escape_unicode().to_string(),
                },
            )
            .collect();
        assert_eq!(folded, "a -- b ... <c> \\u{e9}");
    }

    /// A row is ONE physical line for every line-splitting consumer,
    /// and its VALUE is unchanged — the hostile characters are
    /// re-spelled as `\uXXXX`, never dropped or replaced.
    #[test]
    fn wire_rows_never_carry_raw_unicode_line_breaks_or_bidi() {
        use serde_json::{Value, json};
        // r84-cov (mutant O3 survived): a NON-BMP character the policy
        // escapes (a tag character, U+E0041) spells as a surrogate PAIR —
        // one `\u` unit would change the value.
        let hostile =
            "k\u{85}l p\u{2028}q r\u{2029}s e\u{202e}f a\u{feff}b n\u{7}m t\u{e0041}u plain\u{e9}";
        let row = json!({"type": "result", "target": hostile, "n": 1});
        let line = super::wire_escaped(&row.to_string());
        for raw in [
            '\u{85}',
            '\u{2028}',
            '\u{2029}',
            '\u{202e}',
            '\u{feff}',
            '\u{7}',
            '\n',
            '\u{e0041}',
        ] {
            assert!(!line.contains(raw), "{raw:?} raw on the wire: {line}");
        }
        assert!(
            line.contains("\\u0085") && line.contains("\\u2028") && line.contains("\\u202e"),
            "{line}"
        );
        assert!(
            line.contains("\\udb40\\udc41"),
            "a non-BMP character is a surrogate pair on the wire: {line}"
        );
        assert!(
            line.contains("plain\u{e9}"),
            "a plain non-ASCII character is left alone: {line}"
        );
        let back: Value = serde_json::from_str(&line).expect("still one JSON value");
        assert_eq!(back, row, "the value is unchanged");
        // A row with nothing to escape is byte-identical to serde's spelling.
        let clean =
            json!({"type": "summary", "exit": 0, "root": {"path": "/ws/t\u{e9}nants"}}).to_string();
        assert_eq!(super::wire_escaped(&clean), clean);
    }

    /// The hook must RETURN while another thread holds the sink —
    /// `try_lock`, never `lock`: a hook that blocked would hang the exit
    /// of a run that panicked while a line was being written (measured:
    /// alarm-killed at 15 s, 0 bytes of stderr). Held
    /// here on the test thread, panicked on another under
    /// `catch_unwind`, the hook's return observed within a bound; the
    /// guard is dropped BEFORE the verdict so a failing verdict cannot
    /// deadlock on its own hook.
    #[test]
    fn the_panic_flush_returns_while_the_sink_is_held_elsewhere() {
        super::flush_on_panic();
        let guard = super::err_sink()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(|| {
                panic!("probe: a panic while the sink is held elsewhere")
            });
            let _ = tx.send(());
        });
        let returned = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
        drop(guard);
        assert!(returned, "the hook blocked on the held sink");
    }

    /// The body of the poisoned-arm pin, run in a CHILD test process
    /// (a hook's flush is observable only on the process's own
    /// stderr): a line buffered on the sink, the sink poisoned by a
    /// thread that panics while holding it, then the process's own
    /// panic — the hook takes the poisoned lock and flushes the line
    /// BEFORE the panic message. A no-op without the marker variable.
    #[test]
    fn the_panic_flush_probe_child() {
        if std::env::var_os("NML_PANIC_FLUSH_PROBE").is_none() {
            return;
        }
        super::flush_on_panic();
        super::err(format_args!("BUFFERED-BEFORE-THE-PANIC"));
        let _ = std::thread::spawn(|| {
            let _held = super::err_sink().lock().expect("not yet poisoned");
            panic!("probe: poisoning the sink");
        })
        .join();
        assert!(super::err_sink().lock().is_err(), "the sink is poisoned");
        panic!("probe: the process's own panic");
    }

    /// The POISONED arm flushes — the child above, driven as a process
    /// under a watchdog (a hook that BLOCKED would deadlock the child on
    /// the sink its own poisoning thread holds): its buffered line reaches
    /// stderr, and before the panic message; the child's test fails as
    /// designed (exit 101).
    #[test]
    fn the_panic_flush_takes_a_poisoned_sink_and_flushes_it() {
        let exe = std::env::current_exe().expect("the test binary");
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "out::tests::the_panic_flush_probe_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("NML_PANIC_FLUSH_PROBE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the child spawns");
        // The watchdog: a reader thread drains stderr (the pipe must not
        // fill), the parent polls for the exit and kills at the bound.
        let mut pipe = child.stderr.take().expect("piped");
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
            bytes
        });
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break Some(status);
            }
            if started.elapsed() > std::time::Duration::from_secs(20) {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let bytes = reader.join().expect("the reader ends");
        let stderr = String::from_utf8_lossy(&bytes);
        let status = status.unwrap_or_else(|| {
            panic!("the child hung: the hook blocked on the poisoned sink: {stderr}")
        });
        let flushed = stderr
            .find("BUFFERED-BEFORE-THE-PANIC")
            .unwrap_or_else(|| panic!("the buffered line never reached stderr: {stderr}"));
        let message = stderr
            .find("the process's own panic")
            .unwrap_or_else(|| panic!("no panic message: {stderr}"));
        assert!(
            flushed < message,
            "flushed before the panic message: {stderr}"
        );
        assert_eq!(status.code(), Some(101), "{stderr}");
    }

    /// The panic flush is installed before anything can
    /// write to the buffered stderr — the first `out::` call in `main`.
    #[test]
    fn the_panic_flush_is_installed_before_any_output_in_main() {
        let src = include_str!("main.rs");
        let main_at = src.find("\nfn main() {").expect("main");
        let body = &src[main_at..];
        let hook = body
            .find("out::flush_on_panic();")
            .expect("main installs the panic flush");
        let first = body.find("out::").expect("main writes");
        assert_eq!(
            hook, first,
            "the hook must precede every out:: call in main"
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::{MAX_SHOWN, admit, per_code_cap};
    use nml_core::diagnostic::{Diagnostic, codes};

    /// The default budget in numbers — one
    /// flooding code prints [`MAX_SHOWN`] less its own share (the
    /// reserve), and a code seen afterwards is admitted from the
    /// reserve. The only admitting test in this binary: the tally is
    /// process-wide.
    #[test]
    fn the_default_budget_prints_max_shown_less_the_reserve_for_one_code() {
        let share = per_code_cap(MAX_SHOWN);
        let flood = Diagnostic::error("flood").with_code(codes::SEALED_FIELD_VIOLATION);
        let printed = (0..MAX_SHOWN + share).filter(|_| admit(&flood)).count();
        assert_eq!(printed, MAX_SHOWN - share);
        let rare = Diagnostic::error("rare").with_code(codes::UNIVERSE_TRUNCATED);
        assert!(admit(&rare), "the reserve admits a code not yet seen");
    }
}
