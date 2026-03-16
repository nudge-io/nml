use std::path::PathBuf;
use std::process;

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
    nml <command> [options] <file|dir>

COMMANDS:
    parse                           Parse an NML file and dump the AST as JSON
    validate                        Validate NML files against model definitions
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
            Err(format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e))
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

    let mut resolver = nml_core::resolver::Resolver::new();
    resolver.register_file(&file);

    let errors = resolver.find_duplicates();
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

    let file = nml_core::parse(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;

    let formatted = nml_fmt::formatter::format(&file);
    std::fs::write(&path, &formatted)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;

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
    let path = PathBuf::from(file_args[0]);
    let source = read_file(&path)?;

    let file = nml_core::parse(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;

    let mut resolver = nml_core::resolver::Resolver::new();
    resolver.register_file(&file);

    let source_map = nml_core::span::SourceMap::new(&source);
    let mut error_count = 0;

    for err in resolver.find_duplicates() {
        let loc = source_map.location(err.span().start);
        eprintln!("{}:{}:{}: {}", path.display(), loc.line, loc.column, err);
        error_count += 1;
    }

    if let Some(sd) = schema_dir {
        let (models, enums) = load_schema_dir(&sd)?;
        if !models.is_empty() || !enums.is_empty() {
            let validator = nml_validate::schema::SchemaValidator::new(models, enums);
            for diag in validator.validate(&file) {
                if let Some(span) = diag.span {
                    let loc = source_map.location(span.start);
                    let prefix = match diag.severity {
                        nml_validate::diagnostics::Severity::Error => "error",
                        nml_validate::diagnostics::Severity::Warning => "warning",
                    };
                    eprintln!("{}:{}:{}: {}: {}", path.display(), loc.line, loc.column, prefix, diag.message);
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

fn load_schema_dir(dir: &PathBuf) -> Result<(Vec<nml_core::model::ModelDef>, Vec<nml_core::model::EnumDef>), String> {
    let mut models = Vec::new();
    let mut enums = Vec::new();

    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read schema dir {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "nml") {
            let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if filename.ends_with(".model.nml") {
                let source = read_file(&path)?;
                let file = nml_core::parse(&source).map_err(|e| {
                    format!("failed to parse schema {}: {e}", path.display())
                })?;
                let schema = nml_core::model_extract::extract(&file);
                models.extend(schema.models);
                enums.extend(schema.enums);
            }
        }
    }

    Ok((models, enums))
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
