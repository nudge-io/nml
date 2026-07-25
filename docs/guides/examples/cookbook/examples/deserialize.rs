//! Recipe: deserialize NML blocks into Rust structs with serde.
use nml_core::de::from_body;
use nml_core::{Document, parse};
use serde::Deserialize;

#[derive(Deserialize, Debug, PartialEq)]
struct Service {
    host: String,
    port: u16, // exact: fractional or out-of-range input is a typed error
    #[serde(default)]
    tags: Vec<String>,
    database: Database,
}

#[derive(Deserialize, Debug, PartialEq)]
struct Database {
    url: String,
    #[serde(rename = "poolSize")]
    pool_size: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"
service Api:
    host = "0.0.0.0"
    port = 8080
    tags = ["edge", "public"]

    database:
        url = "postgres://localhost/api"
        poolSize = 10
"#;

    let file = parse(source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().ok_or("no Api block")?;

    let service: Service = from_body(body)?;
    assert_eq!(service.port, 8080);
    assert_eq!(service.database.pool_size, 10);
    assert_eq!(service.tags, ["edge", "public"]);

    println!("recipe OK: deserialize");
    Ok(())
}
