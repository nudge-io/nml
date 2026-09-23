//! Which files a directive vocabulary covers (RFC 0030 coverage, the
//! kernel's answer): schema sources only.

use super::*;
use crate::file_names::SCHEMA_SOURCE_SUFFIXES;
use crate::workspace::VocabularyOutcome;

/// A declared `[]schema` source is covered and never a sibling; a
/// `*.model.nml` beside the manifest but undeclared is covered as an
/// undeclared sibling; an INSTANCE file the manifest binds — beside the
/// manifest or below it — is never judged under a vocabulary (it validates
/// under its binding's schema); a plain file no glob names is opaque.
#[test]
fn vocabulary_coverage_applies_to_schema_sources_only() {
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("apps", &["app.nml", "apps/*.nml"])],
        )
        .file("app.nml")
        .file("apps/x.nml")
        .text("extra.model.nml", "model extra:\n    v string\n")
        .file("notes.nml");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let key_of = |name: &str| {
        d.files
            .iter()
            .find(|k| k.as_str() == name)
            .unwrap_or_else(|| panic!("{name} discovered: {:?}", files(&d)))
    };
    for instance in ["app.nml", "apps/x.nml", "notes.nml"] {
        assert!(
            matches!(
                d.vocabulary_for(key_of(instance)),
                VocabularyOutcome::Opaque
            ),
            "{instance}: an instance or plain file is never judged under a vocabulary"
        );
    }
    match d.vocabulary_for(key_of("core.model.nml")) {
        VocabularyOutcome::Covered(m) => {
            assert!(!m.undeclared_sibling, "declared in []schema");
            assert_eq!(m.vocabulary.package_name(), "demo");
            assert!(m.vocabulary.declared().is_empty());
            assert!(m.vocabulary.get("sealed").is_some_and(|e| e.builtin));
        }
        other => panic!("{other:?}"),
    }
    match d.vocabulary_for(key_of("extra.model.nml")) {
        VocabularyOutcome::Covered(m) => {
            assert!(m.undeclared_sibling, "beside the manifest, undeclared")
        }
        other => panic!("{other:?}"),
    }
}

/// RFC 0026 decision 6: the kernel's spelling of a schema source admits
/// both suffixes and nothing else, and the stem is the name before either
/// — the editor's registry scope reads it, so a `.schema.nml` and a
/// `.model.nml` of one stem are one scope.
#[test]
fn schema_source_stem_admits_both_spellings_and_nothing_else() {
    assert_eq!(schema_source_stem("core.model.nml"), Some("core"));
    assert_eq!(schema_source_stem("core.schema.nml"), Some("core"));
    assert_eq!(schema_source_stem("tenants/cu/plain.flow.nml"), None);
    assert_eq!(schema_source_stem("core.nml"), None);
    assert_eq!(schema_source_stem("demo.package.nml"), None);
    assert_eq!(
        schema_source_stem("model.nml"),
        None,
        "the suffix needs its dot"
    );
    for suffix in SCHEMA_SOURCE_SUFFIXES {
        assert!(is_schema_source_name(&format!("x{suffix}")), "{suffix}");
    }
    assert!(!is_schema_source_name("x.flow.nml"));
}

/// `VocabularyOutcome::covered` is the verdict verbs' one reading of the
/// answer: the match for a covered file; nothing for an opaque file, for
/// an undetermined universe and for an ambiguous coverage alike — and
/// `note` is what a front end says for the two the author can act on.
#[test]
fn covered_is_the_match_or_nothing() {
    use crate::directives::Vocabulary;
    use crate::workspace::{SchemaUniverse, VocabularyMatch};
    assert!(VocabularyOutcome::Opaque.covered().is_none());
    assert!(VocabularyOutcome::Opaque.note().is_none());
    assert!(VocabularyOutcome::Undetermined.covered().is_none());
    let undetermined = VocabularyOutcome::Undetermined.note().expect("a note");
    assert!(
        undetermined
            .message
            .starts_with("package coverage undetermined")
    );
    let ambiguous = VocabularyOutcome::Ambiguous {
        candidates: vec!["demo".to_string(), "other".to_string()],
    };
    assert!(ambiguous.clone().covered().is_none());
    let note = ambiguous.note().expect("a note");
    assert_eq!(
        note.message,
        "package coverage ambiguous: 2 packages could cover this schema source (demo, other) \
         and none declares it — judged under no vocabulary (every directive accepted); \
         declare it in one package's []schema"
    );
    assert!(matches!(
        note.severity,
        nml_core::diagnostic::Severity::Info
    ));
    assert_eq!(note.span, Some(nml_core::span::Span::empty(0)));
    assert!(note.code.is_none());
    let covered = VocabularyOutcome::Covered(VocabularyMatch {
        vocabulary: Vocabulary::new("demo", Vec::new()),
        undeclared_sibling: true,
        universe: SchemaUniverse::None,
    })
    .covered()
    .expect("the match");
    assert_eq!(covered.vocabulary.package_name(), "demo");
    assert!(covered.undeclared_sibling);
}

/// Root-level coverage is the UNIQUE non-builtin claim that binds a file
/// under the root and can govern the file's directory: two such claims
/// are an ambiguity the kernel NAMES (`Ambiguous { candidates }`, sorted;
/// RFC 0030 D8 read it as opaque, which was silent) — never the first
/// manifest's vocabulary by luck of order, never a silent pass.
#[test]
fn two_coverers_make_a_schema_source_ambiguous_never_first_wins() {
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("apps", &["apps/*.nml"])],
        )
        .manifest(
            "other.package.nml",
            "other",
            &[],
            &[("other", &["other/*.nml"])],
        )
        .file("apps/a.nml")
        .file("other/b.nml")
        .text("top.model.nml", "model top:\n    v string\n");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let top = d
        .files
        .iter()
        .find(|k| k.as_str() == "top.model.nml")
        .unwrap_or_else(|| panic!("top.model.nml discovered: {:?}", files(&d)));
    assert_eq!(d.coverers().len(), 2, "{:?}", d.coverers());
    match d.vocabulary_for(top) {
        VocabularyOutcome::Ambiguous { candidates } => {
            assert_eq!(candidates, ["demo".to_string(), "other".to_string()]);
        }
        other => panic!("two coverers are an ambiguity, not a first-wins: {other:?}"),
    }
}

/// R5′ at the vocabulary seam: a workspace manifest covers keys under its
/// OWN directory only — a vendor manifest below the root never covers a
/// root-level schema source (which stays the root manifest's undeclared
/// sibling), and it does cover its own.
#[test]
fn a_workspace_manifest_covers_its_own_subtree_only() {
    let ws = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("apps", &["apps/*.nml"])],
        )
        .manifest(
            "vendor/vendor.package.nml",
            "vendor",
            &[],
            &[("vapps", &["apps/*.nml"])],
        )
        .file("apps/a.nml")
        .file("vendor/apps/v.nml")
        .text("top.model.nml", "model top:\n    v string\n")
        .text("vendor/stray.model.nml", "model stray:\n    v string\n");
    let root = ws.root();
    let d = ws.discover(&root, vec![]);
    let key_of = |name: &str| {
        d.files
            .iter()
            .find(|k| k.as_str() == name)
            .unwrap_or_else(|| panic!("{name} discovered: {:?}", files(&d)))
    };
    assert_eq!(d.coverers().len(), 2, "{:?}", d.coverers());
    match d.vocabulary_for(key_of("top.model.nml")) {
        VocabularyOutcome::Covered(m) => {
            assert_eq!(
                m.vocabulary.package_name(),
                "demo",
                "the root manifest's, not the vendor's"
            );
            assert!(m.undeclared_sibling);
        }
        other => panic!("a vendor manifest below the root must not reach the root: {other:?}"),
    }
    // Under the vendor's own directory BOTH manifests can govern (the root
    // manifest governs every directory beneath it): two coverers are an
    // ambiguity the kernel names (D8 read it as opaque) — a nested package
    // declares its sources to be judged.
    assert!(
        matches!(
            d.vocabulary_for(key_of("vendor/stray.model.nml")),
            VocabularyOutcome::Ambiguous { ref candidates }
                if candidates == &["demo".to_string(), "vendor".to_string()]
        ),
        "{:?}",
        d.vocabulary_for(key_of("vendor/stray.model.nml"))
    );
}

/// Two root-level packages that could each cover an undeclared schema
/// source — neither declares it — are an AMBIGUITY the kernel names
/// (`Ambiguous { candidates }`, sorted), never a silent `Opaque`: a
/// second package in a root used to switch every undeclared source from
/// judged (NML5000) to accept-anything with no row at all. One package
/// alone covers; a source one of them DECLARES is covered by that one
/// whatever the other binds.
#[test]
fn two_coverers_are_an_ambiguity_never_a_silent_opaque() {
    let two = Ws::new()
        .manifest(
            "other.package.nml",
            "other",
            &[],
            &[("shared", &["shared/**/*.flow.nml"])],
        )
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("flows", &["tenants/**/*.flow.nml"])],
        )
        .file("tenants/cu/plain.flow.nml")
        .file("shared/s.flow.nml")
        .text("stray.model.nml", "model stray:\n    v string\n");
    let root = two.root();
    let d = two.discover(&root, vec![]);
    match d.vocabulary_for(&key("stray.model.nml")) {
        VocabularyOutcome::Ambiguous { candidates } => {
            assert_eq!(
                candidates,
                ["demo".to_string(), "other".to_string()],
                "sorted"
            );
        }
        other => panic!("two coverers: {other:?}"),
    }
    // The declared source is covered by its declaring package alone.
    match d.vocabulary_for(&key("core.model.nml")) {
        VocabularyOutcome::Covered(m) => assert!(!m.undeclared_sibling),
        other => panic!("a declared source: {other:?}"),
    }
    // The candidates are SORTED, not in claims order: a store package
    // (an external claim, always after the workspace's) named before the
    // workspace package alphabetically comes first in the answer.
    let mixed = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("flows", &["tenants/**/*.flow.nml"])],
        )
        .file("tenants/cu/plain.flow.nml")
        .file("shared/s.flow.nml")
        .text("stray.model.nml", "model stray:\n    v string\n");
    let root = mixed.root();
    let store = external(
        "aaa",
        &[],
        &[("shared", &["shared/**/*.flow.nml"])],
        ExternalClass::Store,
    );
    let d = mixed.discover(&root, vec![store]);
    match d.vocabulary_for(&key("stray.model.nml")) {
        VocabularyOutcome::Ambiguous { candidates } => {
            assert_eq!(
                candidates,
                ["aaa".to_string(), "demo".to_string()],
                "sorted, not in claims order (the workspace claim is first)"
            );
        }
        other => panic!("a workspace and a store coverer: {other:?}"),
    }
    let one = Ws::new()
        .manifest(
            "demo.package.nml",
            "demo",
            &[],
            &[("flows", &["tenants/**/*.flow.nml"])],
        )
        .file("tenants/cu/plain.flow.nml")
        .text("stray.model.nml", "model stray:\n    v string\n");
    let root = one.root();
    let d = one.discover(&root, vec![]);
    match d.vocabulary_for(&key("stray.model.nml")) {
        VocabularyOutcome::Covered(m) => {
            assert_eq!(m.vocabulary.package_name(), "demo");
            assert!(m.undeclared_sibling);
        }
        other => panic!("one coverer: {other:?}"),
    }
}
