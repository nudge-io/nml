use std::path::PathBuf;
use std::process;

use nml_validate::diagnostics::{Diagnostic, Severity};
use nml_validate::loader::LoadedSchema;

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
    check [--schema <dir>] <file>   Parse + validate + schema check (CI-friendly)
    help                            Show this help message
    version                         Show version information"
    );
}

fn cmd_parse(args: &[String]) -> Result<(), String> {
    let path = require_file_arg(args, "parse")?;
    let source = read_file(&path)?;

    match nml_core::parse(&source) {
        Ok(file) => {
            let json = serde_json::to_string_pretty(&file)
                .map_err(|e| format!("serialization error: {e}"))?;
            println!("{json}");
            Ok(())
        }
        Err(e) => {
            let source_map = nml_core::span::SourceMap::new(&source);
            let loc = source_map.location(e.span().start);
            Err(format!(
                "{}:{}:{}: {}",
                path.display(),
                loc.line,
                loc.column,
                e
            ))
        }
    }
}

fn cmd_validate(args: &[String]) -> Result<(), String> {
    let path = require_file_arg(args, "validate")?;
    let source = read_file(&path)?;

    let file = nml_core::parse(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let mut errors = symbols.find_duplicates();
    errors.extend(symbols.find_unresolved_references(&file));
    if errors.is_empty() {
        println!("{}: ok", path.display());
        Ok(())
    } else {
        let source_map = nml_core::span::SourceMap::new(&source);
        for err in &errors {
            let loc = source_map.location(err.span().start);
            eprintln!("{}:{}:{}: {}", path.display(), loc.line, loc.column, err);
        }
        Err(format!("{} validation error(s)", errors.len()))
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
    let mut file_args: Vec<&String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--schema" {
            i += 1;
            if i >= args.len() {
                return Err("--schema requires a path argument".to_string());
            }
            schema_dir = Some(PathBuf::from(&args[i]));
        } else {
            file_args.push(&args[i]);
        }
        i += 1;
    }

    if file_args.is_empty() {
        return Err("usage: nml check [--schema <dir>] <file>".to_string());
    }
    if file_args.len() > 1 {
        return Err(format!(
            "usage: nml check [--schema <dir>] <file> (got {} files)",
            file_args.len()
        ));
    }
    let path = PathBuf::from(file_args[0]);
    let source = read_file(&path)?;

    let file = nml_core::parse(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;

    let mut symbols = nml_core::symbols::SymbolTable::new();
    symbols.register_file(&file);

    let source_map = nml_core::span::SourceMap::new(&source);
    let mut error_count = 0;

    for err in symbols.find_duplicates() {
        let loc = source_map.location(err.span().start);
        eprintln!("{}:{}:{}: {}", path.display(), loc.line, loc.column, err);
        error_count += 1;
    }
    for err in symbols.find_unresolved_references(&file) {
        let loc = source_map.location(err.span().start);
        eprintln!("{}:{}:{}: {}", path.display(), loc.line, loc.column, err);
        error_count += 1;
    }

    if let Some(sd) = schema_dir {
        let (schema, schema_diags) = load_schema_dir(&sd)?;

        // Schema-level diagnostics (cycles, duplicates) refer to the schema
        // files, not the checked file; report them against the schema dir.
        for diag in &schema_diags {
            eprintln!("{}: {}", sd.display(), diag);
            if matches!(diag.severity, Severity::Error) {
                error_count += 1;
            }
        }

        if !schema.is_empty() {
            let validator = schema.into_validator();
            for diag in validator.validate(&file) {
                let (line, column) = match diag.span {
                    Some(span) => {
                        let loc = source_map.location(span.start);
                        (loc.line, loc.column)
                    }
                    None => (0, 0),
                };
                eprintln!(
                    "{}:{}:{}: {}: {}",
                    path.display(),
                    line,
                    column,
                    diag.severity,
                    diag.message
                );
                if matches!(diag.severity, Severity::Error) {
                    error_count += 1;
                }
            }
        }
    }

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
fn load_schema_dir(dir: &PathBuf) -> Result<(LoadedSchema, Vec<Diagnostic>), String> {
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

    let mut files = Vec::new();
    for path in paths {
        let source = read_file(&path)?;
        let file = nml_core::parse(&source)
            .map_err(|e| format!("failed to parse schema {}: {e}", path.display()))?;
        files.push(file);
    }

    Ok(nml_validate::loader::load_schema(&files))
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
