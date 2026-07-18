//! Shared test fixtures (feature `test-support`): the canonical demo
//! package used across nml-validate and nml-lsp test suites — one owner
//! instead of triplicated consts.

use crate::package::SchemaPackage;

pub const DEMO_MANIFEST: &str = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1
    rootMarkers:
        - \"demo.nml\"

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"demo.nml\"
            - \"apps/*/app.nml\"
        schemas:
            - core
        strict = true
";

pub const DEMO_CORE: &str = "model core:\n    name string+\n    mode string?\n";

/// [`DEMO_MANIFEST`] plus a directive vocabulary (RFC 0032 author surface):
/// bare `#live`/`#restart` and `#key(ident)` — the canonical shape the LSP's
/// directive-vocabulary surfaces (validation, completion, hover) test
/// against.
pub const DEMO_MANIFEST_WITH_DIRECTIVES: &str = "\
package demo:
    version = \"0.1.0\"
    formatVersion = 1
    rootMarkers:
        - \"demo.nml\"

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - core:
        files:
            - \"demo.nml\"
            - \"apps/*/app.nml\"
        schemas:
            - core
        strict = true

[]directive directives:
    - live:
        arg = \"none\"
        doc = \"Change applies without a restart\"
    - restart:
        arg = \"none\"
        doc = \"Change requires a process restart\"
    - key:
        arg = \"ident\"
        doc = \"Names the element-identity field for set pairing\"
";

/// The directive-vocabulary demo package, loaded.
pub fn demo_package_with_directives() -> SchemaPackage {
    SchemaPackage::from_parts(DEMO_MANIFEST_WITH_DIRECTIVES, |_| Ok(DEMO_CORE.to_string()))
        .expect("directive demo package loads")
}

pub fn demo_package() -> SchemaPackage {
    SchemaPackage::from_parts(DEMO_MANIFEST, |_| Ok(DEMO_CORE.to_string()))
        .expect("demo package loads")
}

/// Publish the demo package into `store` — the supported write path
/// (tamper-style raw fixtures stay hand-written where layout pinning is the
/// point).
pub fn publish_demo(store: &crate::store::Store) -> String {
    let package = demo_package();
    store.publish(&package).expect("publish demo");
    package.content_hash()
}
