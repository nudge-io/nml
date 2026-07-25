//! Recipe: programmatically edit a file without destroying formatting.
//!
//! The CST splice API edits the *lossless* tree: comments, blank lines,
//! and the author's layout survive. It refuses two things by design —
//! sources that don't parse cleanly (splicing into an error-recovered tree
//! would act on a structural guess), and snippets that aren't valid
//! entries (no string-pasting injection: the snippet must parse).
use nml_core::cst::edit::{EntryPosition, insert_entry_at_path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"// deployment config — reviewed by SRE
service Api:
    host = "0.0.0.0"   // public bind

    database:
        url = "postgres://localhost/api"
"#;

    // Insert a new property inside the nested `database:` block. The first
    // path segment addresses the top-level block by KEYWORD (names are
    // user-chosen; the keyword is the stable address); subsequent segments
    // address nested blocks by name. Ambiguity refuses rather than guesses.
    let edited = insert_entry_at_path(
        source,
        &["service", "database"],
        "poolSize = 10",
        EntryPosition::Last,
    )
    .ok_or("edit refused")?;

    // The comments and layout are untouched; only the entry appeared.
    assert!(edited.contains("// deployment config — reviewed by SRE"));
    assert!(edited.contains("// public bind"));
    assert!(edited.contains("poolSize = 10"));

    // A snippet that doesn't parse as an entry is refused outright.
    assert!(
        insert_entry_at_path(
            source,
            &["service"],
            "not valid nml {{{",
            EntryPosition::Last
        )
        .is_none()
    );

    print!("{edited}");
    println!("recipe OK: cst_edit");
    Ok(())
}
