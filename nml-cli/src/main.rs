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
    parse       Parse an NML file and dump the AST as JSON
    validate    Validate NML files against model definitions
    fmt         Format NML files in canonical style
    check       Parse + validate + report (CI-friendly)
    help        Show this help message
    version     Show version information"
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
    let path = require_file_arg(args, "check")?;
    let source = read_file(&path)?;

    let file = nml_core::parse(&source).map_err(|e| {
        let source_map = nml_core::span::SourceMap::new(&source);
        let loc = source_map.location(e.span().start);
        format!("{}:{}:{}: {}", path.display(), loc.line, loc.column, e)
    })?;

    let mut resolver = nml_core::resolver::Resolver::new();
    resolver.register_file(&file);

    let errors = resolver.find_duplicates();
    let decl_count = file.declarations.len();

    if errors.is_empty() {
        println!("{}: ok ({decl_count} declaration(s))", path.display());
        Ok(())
    } else {
        let source_map = nml_core::span::SourceMap::new(&source);
        for err in &errors {
            let loc = source_map.location(err.span().start);
            eprintln!("{}:{}:{}: {}", path.display(), loc.line, loc.column, err);
        }
        Err(format!("{} error(s)", errors.len()))
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
