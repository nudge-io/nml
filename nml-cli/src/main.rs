use std::path::{Path, PathBuf};
use std::process;

use nml_core::diagnostic::{Code, Diagnostic, Severity};
use nml_validate::schema::SchemaValidator;

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
    Err(format!("{} parse error(s)", errors.len()))
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
    eprintln!(
        "{}:{}:{}: {}{}: {}",
        path.display(),
        line,
        column,
        diag.severity,
        code,
        diag.rendered()
    );
    diag.code
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
        eprintln!("error: {e}");
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
    validate                        Validate an NML file for duplicates and unresolved references
    fmt                             Format NML files in canonical style
    check [--schema <dir>] [--strict] <file>
                                    Parse + validate + schema check (CI-friendly);
                                    --strict makes unknown properties/keywords errors
    explain <code>                  Explain a diagnostic code (e.g. nml explain NML2007)
    help                            Show this help message
    version                         Show version information"
    );
}

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
/// source the docs render). Coverage over every code is guaranteed by the
/// index's bidirectional CI guard plus a unit test in `nml-core`.
fn cmd_explain(args: &[String]) -> Result<(), String> {
    let [code] = args else {
        return Err("usage: nml explain <code>   (e.g. nml explain NML2007)".to_string());
    };
    let normalized = code.to_ascii_uppercase();
    match nml_core::diagnostic::explain(&normalized) {
        Some(body) => {
            println!("{normalized}\n\n{body}");
            Ok(())
        }
        None => Err(format!(
            "no such diagnostic code: {code} (codes look like NML2007; see the error index)"
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
    let mut named_sources: Vec<(String, PathBuf, String)> = Vec::new();
    if let Some(sd) = &schema_dir {
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
    let file_is_a_source = file_canon.as_ref().is_some_and(|fc| {
        named_sources
            .iter()
            .any(|(_, p, _)| p.canonicalize().ok().as_ref() == Some(fc))
    });
    if !file_is_a_source {
        // The checked file's load name is its display path — unambiguous
        // against directory basenames for attribution.
        named_sources.push((path.display().to_string(), path.clone(), source.clone()));
    }
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

    if !schema.is_empty() {
        // Definition composition is covered by the single load above —
        // instance-only here, so no finding is ever reported twice.
        let mut validator = SchemaValidator::from(schema).composition_checked_at_load();
        // --strict: unknown properties and unmodeled keywords become
        // errors (CI posture; the same profile package bindings can set).
        if strict {
            validator = validator.strict();
        }
        for diag in validator.validate(&file) {
            first_code = first_code.or(report(&path, &source_map, &diag));
            if matches!(diag.severity, Severity::Error) {
                error_count += 1;
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

/// Parse all `*.model.nml` / `*.schema.nml` files in `dir` and run them
/// through the schema-loading pipeline (inheritance resolution, cycle and
/// duplicate detection).
/// Read a schema directory's sources (`*.model.nml` / `*.schema.nml`),
/// sorted for determinism. Loading happens once, in `cmd_check`'s single
/// schema universe (RFC 0012), so the checked file composes with these.
fn read_schema_dir(dir: &PathBuf) -> Result<Vec<(PathBuf, String)>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read schema dir {}: {e}", dir.display()))?;

    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(".model.nml") || name.ends_with(".schema.nml"))
        })
        .collect();
    paths.sort();

    // `load_schema` later parses over the CST and surfaces any parse error
    // as an attributed diagnostic rather than aborting on the first
    // malformed file — reading here only fails on I/O.
    paths
        .into_iter()
        .map(|p| read_file(&p).map(|text| (p, text)))
        .collect()
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

fn write_file_atomically(path: &PathBuf, contents: &str) -> Result<(), String> {
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
