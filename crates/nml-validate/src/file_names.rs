//! The file-NAME vocabulary of the crate: the one place the suffixes that
//! make a file a package manifest, a project config, a schema source, or
//! an NML document at all are spelled. A LEAF — it reads nothing else in
//! the crate — so the package loader (below the workspace kernel) and the
//! kernel (above the loader) both read it downward, and neither reaches
//! into the other for a suffix: before this file the loader read the
//! kernel's `workspace::paths` for `is_manifest_name`, the one upward
//! arrow in the crate, and the editor kept a copy of two of the rules.
//! Every reader asks here rather than re-spelling one; the workspace
//! facade re-exports the public rules so a front end names them as
//! `nml_validate::workspace::…`, beside the walk that applies them.

/// The suffix that makes a file a package manifest (`<name>.package.nml`).
const MANIFEST_SUFFIX: &str = ".package.nml";

/// The suffix that makes a file an NML document at all.
const NML_SUFFIX: &str = ".nml";

/// The name of a project config, exactly: `nml-project.nml`.
pub const PROJECT_CONFIG_NAME: &str = "nml-project.nml";

/// The stem of a file name spelled as a package manifest — `demo` for
/// `demo.package.nml`, the name the manifest must declare; `None` for any
/// other name. A bare `package.nml` deliberately is not one: the suffix
/// carries its own `.`, so a name that IS `package.nml` is shorter than
/// the suffix and cannot end with it (pinned by
/// `a_bare_package_nml_is_not_a_manifest_name`).
pub(crate) fn manifest_stem(name: &str) -> Option<&str> {
    name.strip_suffix(MANIFEST_SUFFIX)
}

/// Whether a file name is spelled as a package manifest — THE
/// manifest-name rule: the package loader's directory scan
/// (`package::find_manifest`), the walk's classification, the per-kind
/// input cap (`InputKind::of_name`, which the editor reads) and the
/// editor's own document gate ask here rather than re-spelling the
/// suffix. A manifest or a project config is a candidate workspace
/// root's marker.
pub fn is_manifest_name(name: &str) -> bool {
    manifest_stem(name).is_some()
}

/// The suffixes that spell a schema source — the ONE list every consumer
/// of the rule reads ([`schema_source_stem`], [`is_schema_source_name`]).
pub const SCHEMA_SOURCE_SUFFIXES: [&str; 2] = [".model.nml", ".schema.nml"];

/// The stem of a file name spelled as a schema source — `core` for
/// `core.model.nml` and for `core.schema.nml` (the editor's registry
/// scope); `None` for any other name.
pub fn schema_source_stem(name: &str) -> Option<&str> {
    SCHEMA_SOURCE_SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
}

/// Whether a file name is spelled as a schema source (`*.model.nml`,
/// `*.schema.nml`) — the one spelling of that rule: the package LOADER
/// reads it to refuse a declared source spelled outside it (NML2105), the
/// walk's coverage answer reads it, the `--schema` directory scan reads
/// it, and the editor admits exactly these to its registry, its schema
/// passes, directive completion and hover (it keeps no predicate of its
/// own).
pub fn is_schema_source_name(name: &str) -> bool {
    schema_source_stem(name).is_some()
}

/// Whether a file name is spelled as an NML document at all (`*.nml`):
/// what the walk lists as content, what a hidden-directory audit counts,
/// what a skipped link or FIFO is judged by, what the editor keeps fresh.
/// One spelling, so no front end can count a file the walk would not.
pub fn is_nml_name(name: &str) -> bool {
    is_nml_name_bytes(name.as_bytes())
}

/// The same rule over raw bytes, for a listed entry whose name is not
/// UTF-8 — no key can carry it, but the walk still says whether it is
/// `.nml`-shaped content it never judged.
pub(crate) fn is_nml_name_bytes(name: &[u8]) -> bool {
    name.ends_with(NML_SUFFIX.as_bytes())
}
