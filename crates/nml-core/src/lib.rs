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

pub mod ast;
pub mod de;
pub mod error;
pub mod lexer;
pub mod model;
pub mod model_extract;
pub mod money;
pub mod parser;
pub mod project;
pub mod query;
pub mod resolve;
pub mod resolver;
pub mod span;
pub mod template;
pub mod types;

pub use parser::parse;
pub use project::ProjectConfig;
pub use query::Document;
pub use resolve::ValueResolver;
pub use resolver::Resolver;
