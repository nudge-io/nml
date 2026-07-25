//! Recipe: wire a custom secret resolver — a vault, a fixture map, anything.
//!
//! `$ENV.KEY` references mean "externally resolved"; the `ValueResolver`
//! decides the source. `ValueResolver::env()` reads real environment
//! variables in production; any closure works and receives the bare key
//! (`"API_KEY"`). Secrets are reference-only by design — a `secret`-typed
//! field never holds a literal, so credentials cannot live in committed
//! files; fallbacks chain to other references (`$ENV.A | $ENV.A_DEV`).
use std::collections::HashMap;

use nml_core::de::from_body_resolved;
use nml_core::{Document, ValueResolver, parse};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "apiKey")]
    api_key: String,
    #[serde(rename = "dbUrl")]
    db_url: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = r#"
service Api:
    apiKey = $ENV.API_KEY
    dbUrl = $ENV.DATABASE_URL | $ENV.DATABASE_URL_DEV
"#;

    // Stand-in for a vault client: any Fn(&str) -> Option<String>.
    let vault: HashMap<&str, &str> = [
        ("API_KEY", "sk-vault-1"),
        // DATABASE_URL is absent — the fallback chain's second leg resolves.
        ("DATABASE_URL_DEV", "postgres://localhost/dev"),
    ]
    .into();
    let resolver = ValueResolver::new(move |key| vault.get(key).map(|v| v.to_string()));

    let file = parse(source)?;
    let doc = Document::new(&file);
    let body = doc.block("service", "Api").body().ok_or("no Api block")?;
    let config: Config = from_body_resolved(body, &resolver)?;

    assert_eq!(config.api_key, "sk-vault-1");
    assert_eq!(config.db_url, "postgres://localhost/dev");

    println!("recipe OK: secret_resolver");
    Ok(())
}
