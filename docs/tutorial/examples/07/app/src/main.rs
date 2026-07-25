//! Chapter 7: the full embedding pipeline — parse, validate, resolve,
//! default, deserialize — in one small program.

use std::error::Error;
use std::fs;

use nml_core::defaults::from_body_defaulted;
use nml_core::diagnostic::Severity;
use nml_core::query::Document;
use nml_core::schema_index::SchemaIndex;
use nml_core::symbols::SymbolTable;
use nml_core::{ValueResolver, parse};
use nml_validate::loader::load_schema;
use nml_validate::schema::SchemaValidator;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceConfig {
    host: String,
    port: f64,
    public_url: String,
    log_level: String,
    request_timeout: String,
    retries: f64,
    tags: Vec<String>,
    api_key: String,
    database: DatabaseConfig,
    endpoints: Vec<Endpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseConfig {
    url: String,
    pool_size: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    /// Injected from the list item's label (`- Api:` → `"Api"`). Bare
    /// positional items (`- "https://…"`) carry no label, so default it.
    #[serde(default)]
    name: String,
    url: String,
    timeout: String,
    check_interval: String,
    regions: Option<Vec<String>>,
}

fn main() -> Result<(), Box<dyn Error>> {
    // 1. Load the schema — the same files `nml check --schema` reads.
    let schema_src = fs::read_to_string("skylight.model.nml")?;
    let (schema, schema_diags) = load_schema(&[("skylight.model.nml", &schema_src)]);
    if !schema_diags.is_empty() {
        for d in &schema_diags {
            eprintln!("schema: {}", d.rendered_message());
        }
        return Err("schema failed to load".into());
    }

    // 2. Parse the config file.
    let source = fs::read_to_string("app.nml")?;
    let file = parse(&source)?;

    // 3. Validate, and refuse to run on errors — config bugs stop here,
    //    not at 3 a.m.
    let validator = SchemaValidator::new(
        schema.models.clone(),
        schema.enums.clone(),
        schema.oneofs.clone(),
    );
    let diagnostics = validator.validate(&file);
    for d in &diagnostics {
        eprintln!("{}: {}", d.severity, d.rendered_message());
    }
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err("configuration is invalid".into());
    }

    // 4. Decide what references mean: environment first, then dev fallbacks
    //    so a fresh checkout runs without real credentials. `const`
    //    references resolve from a snapshot of the file's declarations.
    let mut symbols = SymbolTable::new();
    symbols.register_file(&file);
    let consts = symbols.resolved_const_snapshot();
    let resolver = ValueResolver::new(|key| {
        std::env::var(key).ok().or_else(|| match key {
            "SKYLIGHT_API_KEY_DEV" => Some("dev-key-not-a-secret".to_string()),
            _ => None,
        })
    })
    .with_symbols(move |name| consts.get(name).cloned());

    // 5. Apply schema defaults and deserialize into your types.
    let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);
    let doc = Document::new(&file);
    let body = doc
        .block("service", "Api")
        .body()
        .ok_or("block `service Api` not found in app.nml")?;
    let config: ServiceConfig = from_body_defaulted(&index, "service", body, &resolver)?;

    println!(
        "Skylight {} on {}:{} — log level {}, retries {}, {} endpoint(s)",
        config.public_url,
        config.host,
        config.port,
        config.log_level,
        config.retries,
        config.endpoints.len(),
    );
    for ep in &config.endpoints {
        let name = if ep.name.is_empty() {
            "(unnamed)"
        } else {
            &ep.name
        };
        println!(
            "  - {} {} (timeout {}, every {}, regions: {})",
            name,
            ep.url,
            ep.timeout,
            ep.check_interval,
            ep.regions
                .as_ref()
                .map_or_else(|| "all".to_string(), |r| r.join("+")),
        );
    }
    println!(
        "  database {} (pool {}) — api key {} chars, tags {:?}, timeout {}",
        config.database.url,
        config.database.pool_size,
        config.api_key.len(),
        config.tags,
        config.request_timeout,
    );
    Ok(())
}
