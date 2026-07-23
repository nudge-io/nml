use std::path::{Path, PathBuf};
use std::process;

use nml_core::diagnostic::{Code, Diagnostic, Severity};
use nml_core::schema::ExtractedSchema;
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
        Err(format!("{} validation error(s)", errors.len()))
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

    if let Some(sd) = schema_dir {
        let LoadedSchemaDir {
            schema,
            diagnostics: schema_diags,
            sources: named_sources,
        } = load_schema_dir(&sd)?;

        // Schema-level diagnostics (parse errors, cycles, duplicates) refer
        // to the schema files, not the checked file: locate each against its
        // attributed source (`diag.source` = the file name, RFC 0030) so it
        // prints as `path:line:col` like every other finding. Cross-source
        // findings no one file owns fall back to the dir-prefixed form.
        for diag in &schema_diags {
            let attributed = diag.source.as_deref().and_then(|name| {
                named_sources
                    .iter()
                    .find(|(p, _)| p.file_name().and_then(|f| f.to_str()) == Some(name))
            });
            match attributed {
                Some((schema_path, text)) => {
                    first_code = first_code.or(report(
                        schema_path,
                        &nml_core::span::SourceMap::new(text),
                        diag,
                    ));
                }
                None => {
                    eprintln!("{}: {}", sd.display(), diag);
                    first_code = first_code.or(diag.code);
                }
            }
            if matches!(diag.severity, Severity::Error) {
                error_count += 1;
            }
        }

        if !schema.is_empty() {
            let mut validator = SchemaValidator::from(schema);
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
/// A loaded schema directory: the composed schema, its load diagnostics, and
/// the named sources (kept so schema diagnostics print `file:line:col`).
struct LoadedSchemaDir {
    schema: ExtractedSchema,
    diagnostics: Vec<Diagnostic>,
    sources: Vec<(PathBuf, String)>,
}

fn load_schema_dir(dir: &PathBuf) -> Result<LoadedSchemaDir, String> {
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

    // Read every schema source; `load_schema` parses over the CST and surfaces
    // any parse error as a diagnostic (reported alongside cycle/duplicate
    // diagnostics) rather than aborting on the first malformed file.
    let sources = paths
        .iter()
        .map(read_file)
        .collect::<Result<Vec<String>, _>>()?;
    // Named sources (RFC 0030): diagnostics attribute the offending file.
    let refs: Vec<(&str, &str)> = paths
        .iter()
        .zip(&sources)
        .map(|(p, s)| {
            (
                p.file_name().and_then(|n| n.to_str()).unwrap_or("schema"),
                s.as_str(),
            )
        })
        .collect();
    let (schema, diagnostics) = nml_validate::loader::load_schema(&refs);
    Ok(LoadedSchemaDir {
        schema,
        diagnostics,
        sources: paths.into_iter().zip(sources).collect(),
    })
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
