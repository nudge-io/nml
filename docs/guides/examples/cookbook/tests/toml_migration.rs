//! Recipe: migrate from TOML — the equivalence proof.
//!
//! The migration page's cheatsheet claims NML expresses the same config;
//! this test EXECUTES the claim: one struct, two sources, asserted equal.
use nml_core::de::from_body;
use nml_core::{Document, parse};
use serde::Deserialize;

#[derive(Deserialize, Debug, PartialEq)]
struct Config {
    name: String,
    port: u16,
    features: Vec<String>,
    limits: Limits,
}

#[derive(Deserialize, Debug, PartialEq)]
struct Limits {
    #[serde(rename = "maxConnections")]
    max_connections: u32,
    #[serde(rename = "timeoutSeconds")]
    timeout_seconds: u32,
}

const TOML_SOURCE: &str = r#"
name = "api"
port = 8080
features = ["gzip", "tls"]

[limits]
maxConnections = 512
timeoutSeconds = 30
"#;

const NML_SOURCE: &str = r#"
service Api:
    name = "api"
    port = 8080
    features = ["gzip", "tls"]

    limits:
        maxConnections = 512
        timeoutSeconds = 30
"#;

#[test]
fn the_same_config_means_the_same_thing_in_both_languages() {
    let from_toml: Config = toml::from_str(TOML_SOURCE).expect("toml parses");

    let file = parse(NML_SOURCE).expect("nml parses");
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().expect("Api block");
    let from_nml: Config = from_body(body).expect("nml deserializes");

    assert_eq!(from_toml, from_nml);
}
