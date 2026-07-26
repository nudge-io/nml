//! Recipe: parse a file and read values with the query API.
use nml_core::{Document, parse};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"
const region = "us-east-1"

service Api:
    host = "0.0.0.0"
    port = 8080
    replicas = 3

service Worker:
    host = "10.0.0.7"
    port = 9090
"#;

    let file = parse(source)?;
    let doc = Document::new(&file);

    // One named block, one property — the fluent path.
    let port = doc.block("service", "Api").property("port").to_i64();
    assert_eq!(port, Some(8080));

    // Every block of a keyword, with its name.
    let services = doc.blocks("service");
    let names: Vec<&str> = services.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, ["Api", "Worker"]);

    // Top-level consts resolve through the same document view.
    let region = doc.const_value("region").as_str().map(str::to_owned);
    assert_eq!(region.as_deref(), Some("us-east-1"));

    // Numbers are exact decimals (RFC 0016): an i64 read of a fractional
    // value would be None, never
    // a silent truncation.
    let replicas = doc.block("service", "Api").property("replicas").to_i64();
    assert_eq!(replicas, Some(3));

    println!("recipe OK: parse_and_query");
    Ok(())
}
