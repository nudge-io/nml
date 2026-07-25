//! Chapter 9: load Skylight's schema package, hash it, and publish it to a
//! store — the same flow a real tool runs as `skylight schema sync`.

use std::error::Error;
use std::path::Path;

use nml_validate::package::SchemaPackage;
use nml_validate::store::{PublishOutcome, Store, hash8};

fn main() -> Result<(), Box<dyn Error>> {
    // Load the package: the .package.nml manifest plus every schema source
    // it names, from the chapter directory.
    let package = SchemaPackage::from_dir(Path::new("."))?;
    let manifest = &package.manifest;
    println!(
        "package {} v{} — {} schema(s), {} validator binding(s)",
        manifest.name,
        manifest.version,
        manifest.schemas.len(),
        manifest.validators.len(),
    );

    // The content hash is the package's identity: length-prefixed,
    // newline-normalized frames over manifest + sources, blake3-hashed.
    // Same bytes, same hash — on every machine.
    let hash = package.content_hash();
    println!("content hash {} (short: {})", hash, hash8(&hash));

    // Publish to a store. Real tools use Store::user() — the per-user store
    // editors read; this demo uses a scratch directory.
    let store = Store::at(std::env::temp_dir().join("skylight-store-demo"));
    let outcome = store.publish(&package)?;
    match outcome {
        PublishOutcome::Published { slot } => println!("published to slot {slot}"),
        PublishOutcome::Unchanged => println!("published already — store is current"),
    }

    for listing in store.list() {
        println!(
            "store has: {} v{} ({}, {} slot(s))",
            listing.name, listing.version, listing.hash8, listing.slot_count,
        );
    }
    Ok(())
}
