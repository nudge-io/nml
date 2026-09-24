//! Recipe: embed the language server — give your CLI a `<tool> lsp`
//! subcommand.
//!
//! One call is the whole subcommand; everything around it here is your
//! tool's own embedded package and its exit discipline. Your users get
//! schema-aware editing against the exact binary they run — diagnostics,
//! completion, hover, quick-fixes, in-editor error explanations — with zero
//! schema sync, because the schema ships inside your tool.
use nml_validate::package::SchemaPackage;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    // Your tool embeds its schema package (usually via include_str! of the
    // manifest and sources).
    let package = match SchemaPackage::from_parts(
        r#"package skylight:
    version = "0.1.0"
    formatVersion = 1
    rootMarkers:
        - "skylight.nml"

[]schema schemas:
    - server:
        file = "server.model.nml"
"#,
        |file| match file {
            "server.model.nml" => Ok("model server:\n    port number = 8080\n".to_string()),
            other => Err(format!("unknown source {other}")),
        },
    ) {
        Ok(package) => package,
        Err(err) => {
            eprintln!("embedded schema package is invalid: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The whole body of `<your-tool> lsp`: serves LSP over stdio until the
    // session ends, and says how it ended. `SessionEnd::exit_code()` is the
    // protocol's own mapping (LSP 3.17 §exit: 1 when `exit` arrived with no
    // `shutdown` before it, 0 for the orderly ending AND for a client that
    // simply closed the pipe) — return it from `main`, or map the ending
    // onto your own exit codes, as `tool_exit` below does.
    let ended = nml_lsp::serve(package).await;

    println!("recipe OK: embed_lsp ({:?})", tool_exit(ended));
    ended.exit_code()
}

/// A tool with its OWN closed set of exit codes maps the ending onto that
/// set instead of returning the protocol's — and matches EXHAUSTIVELY, so
/// an ending added later is a compile error here rather than a silent
/// success. Only your `main` can hand a code to the operating system, so
/// this is the last place the distinction survives.
#[derive(Debug)]
enum ToolExit {
    /// The orderly ending, and a client that simply closed the pipe: this
    /// process's outcome is clean.
    Ok,
    /// `exit` with no `shutdown` before it is the client's error — code 1
    /// in LSP 3.17 §exit, and this tool's own refusal.
    Refused,
}

fn tool_exit(ended: nml_lsp::SessionEnd) -> ToolExit {
    match ended {
        nml_lsp::SessionEnd::Exited | nml_lsp::SessionEnd::Disconnected => ToolExit::Ok,
        nml_lsp::SessionEnd::ExitedWithoutShutdown => ToolExit::Refused,
    }
}
