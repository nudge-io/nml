//! Schema loading, packages, and validation for NML — the layer above
//! [`nml_core`]: load schema definitions ([`loader`]), validate instance
//! files against them ([`schema`]), ship them to users as
//! content-addressed packages ([`package`], [`store`]), and resolve which
//! binding governs a file ([`workspace`]).
//!
//! The most common flow needs both crates; the essentials of `nml_core`'s
//! facade are re-exported below so one dependency covers it end to end:
//! parse → validate → apply defaults → deserialize.

// The workspace kernel reaches the filesystem through safe bindings only
// (`rustix` for the race-free read-through, RFC 0019 item 0 E35): no
// `unsafe` anywhere in this crate, enforced at the crate root.
#![forbid(unsafe_code)]

pub mod directives;
mod file_names;
/// The crate's ONE filesystem leaf (E28/E35): the race-free
/// `openat`-beneath chain, the capped reader, the listing rule and the
/// `PathFs`/`LstatFs` oracle. It draws NO arrow to any other module, and
/// three layers need it — the workspace kernel, `package` (a package
/// directory's manifest and sources) and `store` (a slot pointer) — so
/// it is the CRATE's leaf, not the kernel's. Crate-private: its one
/// PUBLIC spelling is `workspace::{read_beneath, read_leaf, …}`, so the
/// module whose documented subject is which binding governs a file also
/// publishes twenty-two filesystem names. Naming the leaf at its own
/// path is an `apiVersion` move — a line-move set in every record — not
/// a rename.
pub(crate) mod fs;
pub mod glob;
pub mod loader;
pub mod package;
pub mod schema;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod workspace;

// ── Re-exported nml-core facade (the layered-crate pattern) ──────────────
// Everything the full pipeline's signatures name, so the common flow is one
// `use nml_validate::…` root: `parse` returns `File`, `SchemaValidator`
// takes `File` and returns `Diagnostic`s, the defaults family takes a
// `SchemaIndex` + `ValueResolver`. Curated by FLOW, not symmetry — deeper
// core layers (cst, diff, …) are an explicit `nml-core` dependency away.
pub use nml_core::diagnostic::{Diagnostic, Severity};
pub use nml_core::{
    Document, File, SchemaIndex, ValueResolver, apply_defaults, from_body_defaulted,
    from_document_defaulted, parse,
};
