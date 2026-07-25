//! Recipe: embed the language server — give your CLI a `<tool> lsp`
//! subcommand.
//!
//! This is the entire implementation. Your users get schema-aware editing
//! against the exact binary they run — diagnostics, completion, hover,
//! quick-fixes, in-editor error explanations — with zero schema sync,
//! because the schema ships inside your tool.
use nml_validate::package::SchemaPackage;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Your tool embeds its schema package (usually via include_str! of the
    // manifest and sources).
    let package = SchemaPackage::from_parts(
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
    )?;

    // The whole body of `<your-tool> lsp`. Serves LSP over stdio until the
    // editor disconnects (here: immediately, on stdin EOF).
    nml_lsp::serve(package).await;

    println!("recipe OK: embed_lsp");
    Ok(())
}
