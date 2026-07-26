//! The pieces of the check pipeline shared between `nml check` and
//! `nml fix` (RFC 0017 §4.1). A fixer that assembled a different schema
//! universe from the checker's would judge files differently than CI does
//! — so the assembly exists once, here.

use std::path::{Path, PathBuf};

/// Read a schema directory's sources (`*.model.nml` / `*.schema.nml`),
/// sorted for determinism. Loading happens once, in the caller's single
/// schema universe (RFC 0012). Parse errors surface later as attributed
/// diagnostics; reading here only fails on I/O.
pub fn read_schema_dir(dir: &Path) -> Result<Vec<(PathBuf, String)>, String> {
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

    paths
        .into_iter()
        .map(|p| {
            std::fs::read_to_string(&p)
                .map(|text| (p.clone(), text))
                .map_err(|e| format!("failed to read {}: {e}", p.display()))
        })
        .collect()
}

/// One schema universe per check (RFC 0012): the `--schema` directory's
/// sources plus the checked file itself — unless it *is* one of them
/// (path-canonicalized, so the same file reached two ways is never loaded
/// twice and never reported as its own duplicate). Each entry is
/// `(load_name, path, text)`; the checked file's load name is its display
/// path — unambiguous against directory basenames for attribution.
pub fn schema_universe(
    path: &Path,
    source: &str,
    schema_dir: Option<&PathBuf>,
) -> Result<Vec<(String, PathBuf, String)>, String> {
    let mut named_sources: Vec<(String, PathBuf, String)> = Vec::new();
    if let Some(sd) = schema_dir {
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
    let mut file_is_a_source = false;
    for (_, p, text) in &mut named_sources {
        if file_canon.is_some() && p.canonicalize().ok() == file_canon {
            file_is_a_source = true;
            // The caller's text wins over the disk copy: `nml fix` analyzes
            // in-memory candidates mid-round, and the universe must see the
            // same bytes the file-level analysis sees.
            *text = source.to_string();
        }
    }
    if !file_is_a_source {
        named_sources.push((
            path.display().to_string(),
            path.to_path_buf(),
            source.to_string(),
        ));
    }
    Ok(named_sources)
}
