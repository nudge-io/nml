//! Chapter 8: a reload classifier — diff two config versions and decide,
//! per change, whether the running service can absorb it live or must
//! restart. The schema's `#live` directives carry the policy; this program
//! just folds them.

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use nml_core::diff::{ChangeKind, FieldChange, Origin, diff_config};
use nml_core::parse;
use nml_core::query::Document;
use nml_core::schema_index::SchemaIndex;
use nml_core::types::Value;
use nml_validate::loader::load_schema;

/// What one change needs, per the schema's directives: the *nearest*
/// directive wins, reading the field path leaf-to-root; a path with no
/// directive at all is conservatively a restart.
fn classify(change: &FieldChange) -> &'static str {
    let steps: Vec<_> = change.path.field_steps().collect();
    for step in steps.into_iter().rev() {
        for directive in &step.directives {
            match directive.name.as_str() {
                "live" => return "live",
                "restart" => return "restart",
                _ => {}
            }
        }
    }
    "restart"
}

/// Render a value for the report. Secrets never reach this function —
/// `FieldChange::is_secret` short-circuits before values are printed.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        Value::Number(n) => format!("{n}"),
        Value::Bool(b) => format!("{b}"),
        other => format!("{other:?}"),
    }
}

fn describe(change: &FieldChange) -> String {
    if change.is_secret() {
        return "changed (secret — values not shown)".to_string();
    }
    match &change.kind {
        ChangeKind::Added { new } => format!("added {}", render(new)),
        ChangeKind::Removed { old } => format!("removed {}", render(old)),
        ChangeKind::Modified { old, new } => {
            format!("{} -> {}", render(old), render(new))
        }
        ChangeKind::SetDelta { added, removed } => {
            let mut parts = Vec::new();
            parts.extend(added.iter().map(|v| format!("+{}", render(v))));
            parts.extend(removed.iter().map(|v| format!("-{}", render(v))));
            parts.join(", ")
        }
        ChangeKind::OpaqueChanged | ChangeKind::ObjectChanged => "changed".to_string(),
    }
}

fn service_body(source: &str) -> Result<nml_core::ast::Body, Box<dyn Error>> {
    let file = parse(source)?;
    let doc = Document::new(&file);
    let body = doc
        .block("service", "Api")
        .body()
        .ok_or("block `service Api` not found")?;
    Ok(body.clone())
}

fn main() -> Result<(), Box<dyn Error>> {
    let schema_src = fs::read_to_string("skylight.model.nml")?;
    let (schema, _) = load_schema(&[("skylight.model.nml", &schema_src)]);
    let index = SchemaIndex::build(schema.models, schema.enums, schema.oneofs);

    let old_src = fs::read_to_string("app.v1.nml")?;
    let new_src = fs::read_to_string("app.nml")?;
    let old_body = service_body(&old_src)?;
    let new_body = service_body(&new_src)?;

    let changes = diff_config(
        &index,
        "service",
        &[(PathBuf::from("app.v1.nml"), &old_body)],
        &[(PathBuf::from("app.nml"), &new_body)],
    );

    let mut live = 0;
    let mut restart = 0;
    println!("Reload plan (app.v1.nml -> app.nml):");
    for change in &changes {
        let verdict = classify(change);
        match verdict {
            "live" => live += 1,
            _ => restart += 1,
        }
        let origin = match &change.origin {
            Origin::Default => " (schema default)",
            Origin::File { .. } => "",
        };
        println!(
            "  {:<8} {}: {}{}",
            verdict,
            change.path,
            describe(change),
            origin
        );
    }
    if restart > 0 {
        println!("{live} change(s) apply live; {restart} need(s) a restart — restart required.");
    } else {
        println!("all {live} change(s) apply live — no restart needed.");
    }
    Ok(())
}
