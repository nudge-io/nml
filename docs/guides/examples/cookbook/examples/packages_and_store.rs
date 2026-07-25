//! Recipe: build, hash, and publish a schema package; consume it from the
//! store.
//!
//! A package's identity is its content hash — versions are human labels.
//! Publishing is content-addressed and idempotent; consumers (your users'
//! editors, via `nml-lsp`) read the store's `current` pointer.

use nml_validate::package::SchemaPackage;
use nml_validate::store::{Store, hash8};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = r#"package skylight:
    version = "0.1.0"
    formatVersion = 1
    rootMarkers:
        - "skylight.nml"

[]schema schemas:
    - server:
        file = "server.model.nml"
"#;
    let package = SchemaPackage::from_parts(manifest, |file| match file {
        "server.model.nml" => Ok("model server:\n    port number = 8080\n".to_string()),
        other => Err(format!("unknown source {other}")),
    })?;

    // Content-addressed identity: same bytes, same hash, everywhere.
    let hash = package.content_hash();
    println!("skylight {} @ {}", package.manifest.version, hash8(&hash));

    // Publish into a store (per-user in production: `Store::user()`; a
    // scratch directory here so the recipe is hermetic).
    let base = std::env::temp_dir().join(format!("nml-cookbook-{}", std::process::id()));
    let store = Store::at(&base);
    store.publish(&package)?;
    // Idempotent: re-publishing identical content is a no-op, not an error.
    store.publish(&package)?;

    // What a consumer does: read the current slot by package name.
    let current = store.read_current("skylight")?;
    assert_eq!(current.package.content_hash(), hash);

    std::fs::remove_dir_all(&base).ok();
    println!("recipe OK: packages_and_store");
    Ok(())
}
