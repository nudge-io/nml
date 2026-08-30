use std::path::{Path, PathBuf};
use std::process;

use nml_core::diagnostic::{Code, Diagnostic, Severity};
use nml_validate::schema::SchemaValidator;

mod fix;
mod pipeline;

/// Parse a file via the CST, reporting **every** syntactic and semantic error
/// at once (not just the first — exceeding the legacy one-at-a-time UX). Returns
/// the AST when the input is fully valid.
fn parse_or_report_all(path: &Path, source: &str) -> Result<nml_core::ast::File, String> {
    let (file, errors) = nml_core::cst::parse_to_ast_all(source);
    if errors.is_empty() {
        return Ok(file);
    }
    let source_map = nml_core::span::SourceMap::new(source);
    let mut first_code = None;
    for e in &errors {
        first_code = first_code.or(report(path, &source_map, e));
    }
    explain_hint(first_code);
    // The suppressed-count row is an info diagnostic riding the same
    // vec — counting it printed "129 parse error(s)" for a 128-error
    // flood.
    let error_count = errors
        .iter()
        .filter(|e| matches!(e.severity, Severity::Error))
        .count();
    Err(format!("{error_count} parse error(s)"))
}

/// The one diagnostic printer: `path:line:col: <Display>` — `Display` renders
/// severity, the stable `[NML0000]` code when assigned, and the did-you-mean
/// hint derived from the structured suggestion (RFC 0008).
/// After a run's diagnostics, point at the offline explanation for the first
/// coded finding — rustc's "for more information" pattern, printed once.
fn explain_hint(first_code: Option<Code>) {
    if let Some(code) = first_code {
        eprintln!("for more information, run: nml explain {code}");
    }
}

fn report(path: &Path, source_map: &nml_core::span::SourceMap, diag: &Diagnostic) -> Option<Code> {
    let (line, column) = match diag.span {
        Some(span) => {
            let loc = source_map.location(span.start);
            (loc.line, loc.column)
        }
        None => (0, 0),
    };
    // `line:col` already locates the finding — the raw byte-span suffix that
    // `Display` adds for span-less contexts would be noise here.
    let code = diag.code.map(|c| format!("[{c}]")).unwrap_or_default();
    // `path` can be a WALKED schema file (`--schema <dir>` attribution),
    // not only an argv path — a hostile filename must not smuggle
    // terminal escapes (the message itself renders through the
    // sanitizing `Rendered`).
    eprintln!(
        "{}:{}:{}: {}{}: {}",
        sanitized(&path.display().to_string()),
        line,
        column,
        diag.severity,
        code,
        diag.rendered()
    );
    // Secondary locations (RFC 0009) — rustc's `note:` shape, each
    // located in ITS OWN file (`Related.source`, RFC 0019 plan item 2):
    // the checked file's map for same-file notes; a foreign path is read
    // and mapped on first use, cached across this diagnostic's notes.
    let own = path.display().to_string();
    let mut foreign: std::collections::HashMap<&str, Option<nml_core::span::SourceMap>> =
        std::collections::HashMap::new();
    for rel in &diag.related {
        eprintln!(
            "{}",
            note_line(path, source_map, &own, &mut foreign, diag, rel)
        );
    }
    diag.code
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

/// One note's rendered line — `<file>:<line>:<col>: note: <message>`,
/// located in the note's own file; `<file>: note: <message> (bytes
/// <start>..<end>)` when that file cannot be read — never the right
/// file with a wrong range (a foreign span through the checked file's
/// map would be exactly that). Message and path both sanitized.
fn note_line<'d>(
    path: &Path,
    source_map: &nml_core::span::SourceMap,
    own: &str,
    foreign: &mut std::collections::HashMap<&'d str, Option<nml_core::span::SourceMap>>,
    diag: &'d Diagnostic,
    rel: &'d nml_core::diagnostic::Related,
) -> String {
    let foreign_src = diag.related_source(rel).filter(|s| *s != own);
    let Some(src) = foreign_src else {
        let loc = source_map.location(rel.span.start);
        return format!(
            "{}:{}:{}: note: {}",
            sanitized(&path.display().to_string()),
            loc.line,
            loc.column,
            sanitized(&rel.message)
        );
    };
    let map = foreign.entry(src).or_insert_with(|| {
        std::fs::read_to_string(src)
            .ok()
            .map(|t| nml_core::span::SourceMap::new(&t))
    });
    match map {
        Some(map) => {
            let loc = map.location(rel.span.start);
            format!(
                "{}:{}:{}: note: {}",
                sanitized(src),
                loc.line,
                loc.column,
                sanitized(&rel.message)
            )
        }
        None => format!(
            "{}: note: {} (bytes {}..{})",
            sanitized(src),
            sanitized(&rel.message),
            rel.span.start,
            rel.span.end
        ),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "parse" => cmd_parse(&args[2..]),
        "validate" => cmd_validate(&args[2..]),
        "fmt" => cmd_fmt(&args[2..]),
        "check" => cmd_check(&args[2..]),
        "fix" => fix::cmd_fix(&args[2..]),
        "explain" => cmd_explain(&args[2..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "version" | "--version" | "-V" => {
            println!("nml {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            process::exit(1);
        }
    };

    if let Err(e) = result {
        // Err strings embed walked filesystem names (an unreadable
        // subdirectory, a failing schema file) — repo content, sanitized
        // like every other surface that prints it.
        eprintln!("error: {}", sanitized(&e));
        process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "nml - NML configuration language toolkit

USAGE:
    nml <command> [options] <file>

COMMANDS:
    parse                           Parse an NML file and dump the AST as JSON
                                    (numbers: JSON number for integer-form
                                    values within u64, else exact string)
    validate                        Validate an NML file for duplicates and unresolved references
    fmt                             Format NML files in canonical style
    check [--schema <dir>] [--strict] <file>
                                    Parse + validate + schema check (CI-friendly);
                                    --strict makes unknown properties/keywords errors
    fix [--schema <dir>] [--dry-run] <path>...
                                    Apply machine-applicable fixes (migrations,
                                    sole-candidate suggestions) in bulk; directories
                                    are walked for .nml files; --dry-run prints a diff
    explain <code>                  Explain a diagnostic code (e.g. nml explain NML2007)
    explain --list                  List every diagnostic code with its summary
    help                            Show this help message
    version                         Show version information"
    );
}

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
fn cmd_parse(args: &[String]) -> Result<(), String> {
    let path = require_file_arg(args, "parse")?;
    let source = read_file(&path)?;

    let file = parse_or_report_all(&path, &source)?;
    let json =
        serde_json::to_string_pretty(&file).map_err(|e| format!("serialization error: {e}"))?;
    println!("{json}");
    Ok(())
}

fn cmd_validate(args: &[String]) -> Result<(), String> {
    let path = require_file_arg(args, "validate")?;
    let source = read_file(&path)?;

    let file = parse_or_report_all(&path, &source)?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let mut errors = symbols.find_duplicates();
    errors.extend(symbols.find_unresolved_references(&file));
    errors.extend(symbols.find_const_cycles());
    // `uses` clause refs are references too (RFC 0019): `validate` does
    // not compose, but its "unresolved references" contract covers the
    // header clause — same NML2059 wording as `check`'s composing path.
    errors.extend(nml_core::layers::check_uses_refs(
        &path.display().to_string(),
        &file,
    ));
    // Schema definitions in the file get the full loader pipeline (RFC 0011):
    // reserved/duplicate definition names, `is` composition, trait usage,
    // oneof integrity, positional arity, cycles — the same findings loading
    // the file via `--schema` would report. A file with no definitions
    // contributes nothing here.
    let file_name = path.display().to_string();
    let (schema, schema_diags) = nml_validate::loader::load_schema(&[(&file_name, &source)]);
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
        println!("{}: ok", path.display());
        Ok(())
    } else {
        let source_map = nml_core::span::SourceMap::new(&source);
        let mut first_code = None;
        for err in &errors {
            first_code = first_code.or(report(&path, &source_map, err));
        }
        explain_hint(first_code);
        // Warnings (e.g. advisory model-reference cycles) report but do not
        // fail the file — same posture as `check`.
        let error_count = errors
            .iter()
            .filter(|d| d.severity == nml_core::diagnostic::Severity::Error)
            .count();
        if error_count == 0 {
            println!("{}: ok", path.display());
            return Ok(());
        }
        Err(format!("{error_count} validation error(s)"))
    }
}

/// `nml explain NML2007` — the embedded error index (offline; the same
/// source the docs render, via the same `explain_document` composer the
/// editor's `nml/explain` serves, so "the full entry" has exactly one shape).
/// `--list` prints every code with its one-line summary (grep-able).
/// Coverage over every code is guaranteed by the index's bidirectional CI
/// guard plus a unit test in `nml-core`.
fn cmd_explain(args: &[String]) -> Result<(), String> {
    if let [flag] = args {
        if flag == "--list" {
            for (code, summary) in nml_core::diagnostic::explain_index() {
                println!("{code}  {summary}");
            }
            return Ok(());
        }
    }
    let [code] = args else {
        return Err("usage: nml explain <code> | --list   (e.g. nml explain NML2007)".to_string());
    };
    match nml_core::diagnostic::explain_document(&code.to_ascii_uppercase()) {
        Some(document) => {
            // The composed document is trim-ended (the sections are trimmed at
            // the splitter); terminate the terminal line explicitly.
            println!("{document}");
            Ok(())
        }
        None => Err(format!(
            "no such diagnostic code: {code} (codes look like NML2007; see the error index, or `nml explain --list`)"
        )),
    }
}

fn cmd_fmt(args: &[String]) -> Result<(), String> {
    let path = require_file_arg(args, "fmt")?;
    let source = read_file(&path)?;

    let formatted = nml_fmt::formatter::format_source(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;
    write_file_atomically(&path, &formatted)?;

    println!("formatted {}", path.display());
    Ok(())
}

fn cmd_check(args: &[String]) -> Result<(), String> {
    let mut schema_dir: Option<PathBuf> = None;
    let mut strict = false;
    let mut file_args: Vec<&String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--schema" {
            i += 1;
            if i >= args.len() {
                return Err("--schema requires a path argument".to_string());
            }
            schema_dir = Some(PathBuf::from(&args[i]));
        } else if args[i] == "--strict" {
            strict = true;
        } else {
            file_args.push(&args[i]);
        }
        i += 1;
    }

    if file_args.is_empty() {
        return Err("usage: nml check [--schema <dir>] [--strict] <file>".to_string());
    }
    if file_args.len() > 1 {
        return Err(format!(
            "usage: nml check [--schema <dir>] [--strict] <file> (got {} files)",
            file_args.len()
        ));
    }
    let path = PathBuf::from(file_args[0]);
    let source = read_file(&path)?;

    let file = parse_or_report_all(&path, &source)?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let source_map = nml_core::span::SourceMap::new(&source);
    let mut error_count = 0;

    let mut first_code = None;
    for err in symbols
        .find_duplicates()
        .into_iter()
        .chain(symbols.find_unresolved_references(&file))
        .chain(symbols.find_const_cycles())
    {
        first_code = first_code.or(report(&path, &source_map, &err));
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
    // Assembly is shared with `nml fix` (pipeline module), so the fixer
    // can never judge a file differently than this verb does.
    let named_sources = pipeline::schema_universe(&path, &source, schema_dir.as_ref())?;
    let source_refs: Vec<(&str, &str)> = named_sources
        .iter()
        .map(|(n, _, t)| (n.as_str(), t.as_str()))
        .collect();
    let (schema, schema_diags) = nml_validate::loader::load_schema(&source_refs);

    // Attributed findings print `path:line:col` against their declaring
    // source; a finding no single definition owns falls back to a
    // location-less line under the schema dir (or the file).
    for diag in &schema_diags {
        let attributed = diag
            .source
            .as_deref()
            .and_then(|name| named_sources.iter().find(|(n, _, _)| n == name));
        match attributed {
            Some((_, src_path, text)) => {
                first_code = first_code.or(report(
                    src_path,
                    &nml_core::span::SourceMap::new(text),
                    diag,
                ));
            }
            None => {
                match &schema_dir {
                    Some(sd) => eprintln!("{}: {}", sd.display(), diag),
                    None => eprintln!("{}: {}", path.display(), diag),
                }
                first_code = first_code.or(diag.code);
            }
        }
        if matches!(diag.severity, Severity::Error) {
            error_count += 1;
        }
    }

    // `--strict` promises enforcement; with an empty schema universe there
    // is nothing to enforce, and silently degrading to parse-only checking
    // is how a CI pipeline points at the wrong path and stays green
    // forever. Fail the *invocation*, naming the actual mistake.
    if strict && schema.is_empty() {
        return Err(
            "--strict has nothing to enforce: no schema definitions found \
             (no --schema directory given and none declared in the file)"
                .to_string(),
        );
    }

    {
        // RFC 0019: compose `uses` stacks before validation (default on).
        // Same-file stacks in the open developer context; binding-governed
        // grants arrive with the resolver-core slice. Blocks that compose
        // validate their RESOLVED body (an overlay alone is deliberately
        // partial); everything else validates as authored. The pass runs
        // even with no schema universe: NML2059/2061/2062/2077 are
        // structural, not schema-dependent. The validator is built FIRST
        // and its index shared with the layers engine — one index build,
        // zero schema clones, and the merge-policy pass and the validator
        // can never see different schemas.
        let validator = (!schema.is_empty()).then(|| {
            let mut v = SchemaValidator::from(schema).composition_checked_at_load();
            if strict {
                v = v.strict();
            }
            v
        });
        let empty_index = nml_core::schema_index::SchemaIndex::build(vec![], vec![], vec![]);
        let index = validator.as_ref().map_or(&empty_index, |v| v.index());
        let source_name = path.display().to_string();
        let composed = nml_core::layers::compose_file(
            index,
            &source_name,
            &file,
            &nml_core::layers::OpenContext,
        );
        for diag in &composed.diagnostics {
            first_code = first_code.or(report(&path, &source_map, diag));
            if matches!(diag.severity, Severity::Error) {
                error_count += 1;
            }
        }
        let validation_file = composed.validation_file;

        if let Some(validator) = &validator {
            // Definition composition is covered by the single load above —
            // instance-only here, so no finding is ever reported twice
            // across passes; and one home per finding within this pass — a
            // resolved overlay body carries clones of base entries at their
            // authored spans, so a base defect would otherwise report once
            // as authored and once per overlay. Identical (code, span,
            // message) triples collapse to one. SEEDED with the composed
            // diagnostics already reported above: the merge emits some
            // validator-shaped findings itself (a bogus `as` the composed
            // view would otherwise swallow, NML2051), and a non-`uses`
            // base declaration's raw validation re-derives the same
            // finding at the same span — the LSP and `nml fix` seed this
            // way too, and an unseeded set printed the pair twice here.
            let mut seen: std::collections::HashSet<nml_core::layers::FindingKey> = composed
                .diagnostics
                .iter()
                .map(nml_core::layers::finding_key)
                .collect();
            for diag in validator.validate(validation_file.as_ref().unwrap_or(&file)) {
                if !seen.insert(nml_core::layers::finding_key(&diag)) {
                    continue;
                }
                first_code = first_code.or(report(&path, &source_map, &diag));
                if matches!(diag.severity, Severity::Error) {
                    error_count += 1;
                }
            }
        }
    }

    explain_hint(first_code);
    let decl_count = file.declarations.len();
    if error_count == 0 {
        println!("{}: ok ({decl_count} declaration(s))", path.display());
        Ok(())
    } else {
        Err(format!("{error_count} error(s)"))
    }
}

fn require_file_arg(args: &[String], cmd: &str) -> Result<PathBuf, String> {
    if args.is_empty() {
        return Err(format!("usage: nml {cmd} <file>"));
    }
    Ok(PathBuf::from(&args[0]))
}

fn read_file(path: &PathBuf) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

pub(crate) fn write_file_atomically(path: &PathBuf, contents: &str) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| {
        format!(
            "failed to determine parent directory for {}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid file name for {}", path.display()))?;
    let tmp_name = format!(".{}.tmp-{}", file_name, std::process::id());
    let tmp_path = parent.join(tmp_name);

    std::fs::write(&tmp_path, contents)
        .map_err(|e| format!("failed to write temp file {}: {e}", tmp_path.display()))?;
    // Preserve the original's permission bits: the temp file is created at
    // the umask default, and the rename would otherwise silently widen a
    // restricted config (0600 → 0644) — a rewrite must never change who
    // can read the file. A file being created fresh keeps the default.
    if let Ok(meta) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp_path, meta.permissions()).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            format!("failed to preserve permissions on {}: {e}", path.display())
        })?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!(
            "failed to replace {} with {}: {e}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_core::span::Span;

    /// `Related.source` rendering (RFC 0019 plan item 2), pinned at the
    /// renderer because both consumers compose single-file today: a
    /// same-file note through the checked map; a foreign note through
    /// ITS OWN file's map; an unreadable path without a range — never
    /// the right file with a wrong range.
    #[test]
    fn notes_locate_in_their_own_files() {
        let checked_text = "a = 1\n";
        let map = nml_core::span::SourceMap::new(checked_text);
        let path = Path::new("main.nml");
        let own = path.display().to_string();
        let mut foreign = std::collections::HashMap::new();

        let dir = std::env::temp_dir().join(format!("nml-note-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let b = dir.join("b.nml");
        std::fs::write(&b, "x = 1\ny = 2\n    z = 3\n").unwrap();
        let b_name = b.display().to_string();

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
            .map(|rel| note_line(path, &map, &own, &mut foreign, &diag, rel))
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
        std::fs::remove_dir_all(&dir).ok();
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
}
