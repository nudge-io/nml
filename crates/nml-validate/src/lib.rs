//! Schema loading, packages, and validation for NML — the layer above
//! [`nml_core`]: load schema definitions ([`loader`]), validate instance
//! files against them ([`schema`]), ship them to users as
//! content-addressed packages ([`package`], [`store`]), and resolve which
//! binding governs a file ([`workspace`]) — every read on the way through
//! the crate's one filesystem leaf ([`fs`]).
//!
//! The flow spans both crates and this one re-exports none of the other:
//! parse and deserialize are `nml_core`'s (`parse`, the defaults family,
//! `Diagnostic`), validation and resolution are this crate's. Name each at
//! its own crate — one `use` line more, and no name with two homes.

// The workspace kernel reaches the filesystem through safe bindings only
// (`rustix` for the race-free read-through, RFC 0019 item 0 E35): no
// `unsafe` anywhere in this crate, enforced at the crate root.
#![forbid(unsafe_code)]

pub mod directives;
mod file_names;
// The crate's ONE filesystem leaf, documented in its own root (`fs/mod.rs`):
// an outer doc comment here would make rustdoc resolve the leaf's own links
// in THIS scope, where none of its names are.
pub mod fs;
pub mod glob;
pub mod loader;
pub mod package;
pub mod schema;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod workspace;

// No nml-core facade: every consumer names `nml_core::…` for the parse,
// the defaults family and the diagnostic types (the platform does, 358
// times), and the ten-name re-export here had no caller anywhere — the
// public-API record was its only reader (removed at apiVersion 5).
