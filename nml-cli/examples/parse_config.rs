//! Parse an NML config file and extract values using the query API.
//!
//! Run with: cargo run --example parse_config

use nml_core::{parse, Document};

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
    let port = doc
        .block("service", "WebApp")
        .property("port")
        .as_f64()
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

    let retries = doc.const_value("MaxRetries").as_f64().unwrap_or(3.0);
    println!("Max retries: {retries}");

    println!("\nAll declarations:");
    for (keyword, name) in doc.declarations() {
        println!("  {keyword} {name}");
    }
}
