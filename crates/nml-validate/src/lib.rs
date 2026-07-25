//! Schema loading, packages, and validation for NML — the layer above
//! [`nml_core`]: load schema definitions ([`loader`]), validate instance
//! files against them ([`schema`]), and ship them to users as
//! content-addressed packages ([`package`], [`store`]).
//!
//! The most common flow needs both crates; the essentials of `nml_core`'s
//! facade are re-exported below so one dependency covers it end to end:
//! parse → validate → apply defaults → deserialize.

pub mod glob;
pub mod loader;
pub mod package;
pub mod schema;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// ── Re-exported nml-core facade (the layered-crate pattern) ──────────────
// Everything the full pipeline's signatures name, so the common flow is one
// `use nml_validate::…` root: `parse` returns `File`, `SchemaValidator`
// takes `File` and returns `Diagnostic`s, the defaults family takes a
// `SchemaIndex` + `ValueResolver`. Curated by FLOW, not symmetry — deeper
// core layers (cst, diff, …) are an explicit `nml-core` dependency away.
pub use nml_core::diagnostic::{Diagnostic, Severity};
pub use nml_core::{
    apply_defaults, from_body_defaulted, from_document_defaulted, parse, Document, File,
    SchemaIndex, ValueResolver,
};
