//! Deserialize NML blocks into typed Rust structs using serde.
//!
//! Run with: cargo run --example serde_deserialize

use nml_core::de::from_body;
use nml_core::{Document, parse};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct ServiceConfig {
    host: String,
    port: f64,
    debug: bool,
    tags: Vec<String>,
}

fn main() {
    let source = r#"
service WebApp:
    host = "0.0.0.0"
    port = 8080
    debug = true
    tags = ["web", "api"]

service Worker:
    host = "0.0.0.0"
    port = 9090
    debug = false
    tags = ["worker", "background"]
"#;

    let file = parse(source).expect("failed to parse NML");
    let doc = Document::new(&file);

    for (name, block) in doc.blocks("service") {
        let body = block.body().expect("block should have body");
        let config: ServiceConfig = from_body(body).expect("failed to deserialize");
        println!(
            "{name}: listening on {}:{} (debug={}, tags={:?})",
            config.host, config.port, config.debug, config.tags
        );
    }
}
