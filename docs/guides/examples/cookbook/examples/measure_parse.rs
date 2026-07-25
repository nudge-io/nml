//! Recipe support: the footprint page's parse measurement, reproducible by
//! anyone with the repo (`cargo run --release -p nml-cookbook --example
//! measure_parse`). Walks the repo's .nml corpus and parses every file with
//! the resilient parser (the corpus deliberately includes invalid fixtures —
//! error recovery is part of the work being measured). Prints throughput;
//! asserts only corpus size, never timing (CI must not flake on a slow
//! runner).
use std::time::Instant;

fn repo_root() -> std::path::PathBuf {
    // examples/cookbook is four levels below the repo root.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo root exists")
        .to_path_buf()
}

fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "nml") {
            out.push(path);
        }
    }
}

fn main() {
    let root = repo_root();
    let mut files = Vec::new();
    for corpus in [
        "tests/fixtures",
        "spec/examples",
        "docs/tutorial/examples",
        "docs/errors",
        "docs/guides",
    ] {
        collect(&root.join(corpus), &mut files);
    }
    files.sort();
    assert!(
        files.len() >= 30,
        "corpus went missing? found {}",
        files.len()
    );

    let sources: Vec<String> = files
        .iter()
        .map(|p| std::fs::read_to_string(p).expect("corpus file reads"))
        .collect();
    let total_bytes: usize = sources.iter().map(String::len).sum();

    let start = Instant::now();
    let mut diagnostics = 0usize;
    for source in &sources {
        let (_file, diags) = nml_core::cst::parse_to_ast_all(source);
        diagnostics += diags.len();
    }
    let elapsed = start.elapsed();

    let mibps = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    println!(
        "parsed {} files ({} bytes) in {:?} — {:.1} MiB/s (resilient parse, {} diagnostics)",
        files.len(),
        total_bytes,
        elapsed,
        mibps,
        diagnostics
    );
    println!("recipe OK: measure_parse");
}
