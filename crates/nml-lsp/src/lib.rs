// No `unsafe` in this crate (RFC 0019 item 0, E35): enforced at the root.
#![forbid(unsafe_code)]

//! The NML language server. Two entry points and one result make up the
//! public surface: [`serve`] is the whole body of a provider tool's
//! `<tool> lsp` subcommand (RFC 0035), [`serve_stdio`] is the neutral
//! server the `nml-lsp` binary runs, and both return [`SessionEnd`] — how
//! the session ended, for the process to map onto its exit code. Every
//! other item is the crate's own; the integration harness and the CLI's
//! parity test reach the ones they drive through the `test-support`
//! feature, which is never part of the recorded API.

pub(crate) mod ask;
mod diagnostics;
mod duration_lsp;
mod packages;
mod position;
#[cfg(test)]
mod scratch;
mod semantic_tokens;
mod server;
mod session;
mod transport;
// The wasm editor's directory listings and their memo. Compiled under
// `test` on every target too, so the wiring cannot rot uncompiled.
#[cfg(any(target_os = "wasi", test))]
mod wasi_fs;

pub use session::SessionEnd;
pub use transport::{serve, serve_stdio};

/// What the integration harness (`tests/harness.rs`) and the CLI's parity
/// test drive directly: the service builder with its flavors, the
/// resolver, and the two server constants they pin. Enabled by those test
/// crates alone (a self dev-dependency here, a dev-dependency in nml-cli),
/// so none of it is in the recorded public API.
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::packages::{OpenDocuments, PackageResolver, Resolution, WorkspaceView};
    pub use crate::server::{MAX_INDEX_BYTES, NmlLanguageServer, SERVER_NAME};
    pub use crate::session::{ExitSignal, NmlService, build_service};
}
