//! Recipe: format user files idempotently, preserving comments.
//!
//! `format_source` is the whole API: canonical style out, comments kept.
//! Idempotence (format(format(x)) == format(x)) is what makes it safe to
//! run on every save or in a pre-commit hook. Write atomically in your
//! tool (temp file + rename) so a crash never truncates a user's config.
use nml_fmt::formatter::format_source;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let messy = "service   Api:\n    // keep me\n    port=8080\n    host    =  \"0.0.0.0\"\n";

    let once = format_source(messy)?;
    let twice = format_source(&once)?;

    assert_eq!(once, twice, "formatting must be idempotent");
    assert!(once.contains("// keep me"), "comments are preserved");
    assert!(once.contains("port = 8080"), "canonical spacing");

    print!("{once}");
    println!("recipe OK: format_files");
    Ok(())
}
