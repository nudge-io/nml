//! Deserialize NML blocks into typed Rust structs using serde.
//!
//! Run with: cargo run --example serde_deserialize

use nml_core::de::from_block;
use nml_core::{parse, Document};
use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
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
        let config: ServiceConfig = from_block(body).expect("failed to deserialize");
        println!("{name}: {config:#?}");
    }
}
