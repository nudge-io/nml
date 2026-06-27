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
//!     .as_f64();
//! assert_eq!(port, Some(8080.0));
//! ```
//!
//! # Serde Integration
//!
//! Use the [`de`] module to deserialize NML blocks directly into Rust structs:
//!
//! ```rust
//! use serde::Deserialize;
//! use nml_core::{parse, Document};
//! use nml_core::de::from_block;
//!
//! #[derive(Deserialize)]
//! struct Config {
//!     port: f64,
//!     host: String,
//! }
//!
//! let source = "service MyApp:\n    port = 8080\n    host = \"localhost\"\n";
//! let file = parse(source).unwrap();
//! let doc = Document::new(&file);
//! let body = doc.block("service", "MyApp").body().unwrap();
//! let config: Config = from_block(body).unwrap();
//! ```

/// The typed **semantic AST** (`File`/`Declaration`/decoded `Value`s …) — the
/// model that semantic consumers (validation, deserialization, defaulting) read.
/// Produced by lowering the lossless [`cst`] (see [`cst::lower`]) — the production
/// parse path that supersedes the legacy parser (which now survives only in
/// nml-core's own tests, pending removal).
pub mod ast;
/// RFC 0004 lossless CST: the production parser (resilient red/green tree with
/// exact spans, trivia, and comments). Tooling that needs losslessness/resilience
/// reads this directly; semantic consumers read the [`ast`] it lowers to.
pub mod cst;
pub mod de;
pub mod defaults;
pub mod error;
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
pub mod span;
pub mod symbols;
pub mod template;
pub mod types;

/// The top-level parse facade: source → semantic [`ast::File`], reporting the
/// first error. Ergonomic alias for [`cst::parse_to_ast`] (the layered name).
pub use cst::parse_to_ast as parse;
pub use defaults::{apply_defaults, from_block_defaulted, from_body_defaulted};
pub use project::ProjectConfig;
pub use query::Document;
pub use resolve::ValueResolver;
pub use schema_index::{FieldTarget, SchemaIndex};
pub use symbols::SymbolTable;
