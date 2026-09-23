//! NML -- A typed configuration language.
//!
//! `nml-core` provides parsing, AST representation, and value extraction
//! for NML configuration files. It is designed to be used as a library
//! by any Rust project that wants to use NML as its configuration format.
//!
//! # Quick Start
//!
//! ```rust
//! use nml_core::{parse, Document};
//!
//! let source = r#"
//! service MyApp:
//!     port = 8080
//!     host = "localhost"
//! "#;
//!
//! let file = parse(source).unwrap();
//! let doc = Document::new(&file);
//!
//! let port = doc.block("service", "MyApp")
//!     .property("port")
//!     .to_i64();
//! assert_eq!(port, Some(8080));
//! ```
//!
//! # Serde Integration
//!
//! Use the [`de`] module to deserialize NML blocks directly into Rust structs:
//!
//! ```rust
//! use serde::Deserialize;
//! use nml_core::{parse, Document};
//! use nml_core::de::from_body;
//!
//! #[derive(Deserialize)]
//! struct Config {
//!     port: u16,
//!     host: String,
//! }
//!
//! let source = "service MyApp:\n    port = 8080\n    host = \"localhost\"\n";
//! let file = parse(source).unwrap();
//! let doc = Document::new(&file);
//! let body = doc.block("service", "MyApp").body().unwrap();
//! let config: Config = from_body(body).unwrap();
//! ```

/// The typed **semantic AST** (`File`/`Declaration`/decoded `Value`s …) — the
/// model that semantic consumers (validation, deserialization, defaulting) read.
/// Produced by lowering the lossless [`cst`] through its one parse funnel
/// (`cst::parse_to_ast` and its siblings; the lowering itself is
/// crate-private, so no AST bypasses the rules emitted beside it) — the
/// production parse path (the pre-CST legacy parser is long removed).
pub mod ast;
/// RFC 0004 lossless CST: the production parser (resilient red/green tree with
/// exact spans, trivia, and comments). Tooling that needs losslessness/resilience
/// reads this directly; semantic consumers read the [`ast`] it lowers to.
pub mod cst;
pub mod de;
/// The exact decimal numeric core (RFC 0016): NML's number domain is the
/// finite decimal128 value space, error-on-inexact. One shared `const`
/// parse path serves literals, `FromStr`, env-string coercion, and the
/// compile-time-checked [`num!`](crate::num) macro.
pub mod decimal;
pub mod defaults;
/// The unified diagnostics model (RFC 0008): one `Diagnostic` type — with
/// stable codes, severities, spans, and machine-applicable suggestions —
/// shared by every finding-reporting surface (validator, symbols, parse
/// error lists, LSP, CLI). `error::NmlError` remains the thin `Result`
/// abort error; this module is the findings report.
pub mod diagnostic;
pub mod diff;
pub mod duration;
/// The kernel's rule over names (NML1000 at the file scope, NML2093 in a
/// body): a scope declares each name once — run beside every parse, so no
/// consumer can skip it.
mod entry_names;
pub mod error;
pub mod identity;
pub mod layers;
pub mod model;
pub mod money;
pub mod project;
pub mod query;
pub mod resolve;
/// The assembled schema (`ExtractedSchema` = models + enums + oneofs, produced by
/// [`cst::extract`]) and the passes over it: inheritance resolution and
/// `extends`/model-reference cycle + `oneof` integrity detection. `model` holds
/// the leaf definitions; this holds the aggregate and the checks.
pub mod schema;
pub mod schema_index;
pub mod source_policy;
pub mod span;
/// The near-miss suggestion engine (RFC 0008; formerly in nml-validate) —
/// one metric and one policy behind every "did you mean" hint.
pub mod suggest;
pub mod symbols;
pub mod template;
pub mod types;

/// [`parse`]'s own return type belongs on the facade beside it.
pub use ast::File;
/// The top-level parse facade: source → semantic [`ast::File`], reporting the
/// first error. Ergonomic alias for [`cst::parse_to_ast`] (the layered name).
pub use cst::parse_to_ast as parse;
pub use defaults::{
    apply_defaults, from_block_defaulted, from_body_defaulted, from_document_defaulted,
};
pub use project::ProjectConfig;
pub use query::Document;
pub use resolve::ValueResolver;
pub use schema_index::{FieldTarget, SchemaIndex};
pub use symbols::SymbolTable;
