//! Parse an NML config file and extract values using the query API.
//!
//! Run with: cargo run --example parse_config

use nml_core::{Document, parse};

fn main() {
    let source = r#"
service WebApp:
    host = "0.0.0.0"
    port = 8080
    debug = true
    tags = ["web", "api", "v2"]

    database:
        url = "postgres://localhost/myapp"
        pool_size = 10

const MaxRetries = 5
"#;

    let file = parse(source).expect("failed to parse NML");
    let doc = Document::new(&file);

    let host = doc
        .block("service", "WebApp")
        .property("host")
        .as_str()
        .expect("missing host");
    // Exact integer extraction (RFC 0016): `to_i64` is Some iff the
    // value is exactly this integer — `to_f64` is the lossy binary
    // edge, wrong for ports and counts.
    let port = doc
        .block("service", "WebApp")
        .property("port")
        .to_i64()
        .expect("missing port");
    let debug = doc
        .block("service", "WebApp")
        .property("debug")
        .as_bool()
        .unwrap_or(false);
    let tags = doc
        .block("service", "WebApp")
        .property("tags")
        .as_string_array()
        .unwrap_or_default();

    println!("Host:  {host}");
    println!("Port:  {port}");
    println!("Debug: {debug}");
    println!("Tags:  {}", tags.join(", "));

    let db_url = doc
        .block("service", "WebApp")
        .nested("database")
        .property("url")
        .as_str()
        .expect("missing database url");
    println!("DB:    {db_url}");

    let retries = doc.const_value("MaxRetries").to_i64().unwrap_or(3);
    println!("Max retries: {retries}");

    println!("\nAll declarations:");
    for (keyword, name) in doc.declarations() {
        println!("  {keyword} {name}");
    }
}
