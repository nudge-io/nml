//! The A7 table's totality and the disclosure rules of its messages.

use super::*;
use crate::workspace::diag::{code_for, path_finding};
use crate::workspace::discover::MAX_TOTAL_LIVE_INPUT_BYTES;
use nml_core::diagnostic::{Suggestion, codes};

/// Every `PathError` shape. The match below is what makes this list
/// total: a new variant fails to compile here before it can ship
/// without a row.
fn every_shape() -> Vec<PathError> {
    let shapes = vec![
        PathError::NotRelative,
        PathError::Escapes {
            authored: "tenants/cu/lib/../../admin/secret.nml".into(),
        },
        PathError::Depth,
        PathError::NotUtf8,
        PathError::NotPlain {
            component: "ev\\il".into(),
        },
        PathError::SymlinkComponent {
            component: "lib".into(),
            key: key("tenants/cu/lib/x.nml"),
        },
        PathError::Unverifiable {
            key: key("ADMIN/s.nml"),
        },
        PathError::Fs(FsError::Denied),
        PathError::Fs(FsError::SymlinkLoop),
        PathError::Fs(FsError::NoRealpath),
        PathError::Fs(FsError::Io(Some(5))),
        PathError::Fs(FsError::Io(None)),
    ];
    for shape in &shapes {
        match shape {
            PathError::NotRelative
            | PathError::Escapes { .. }
            | PathError::Depth
            | PathError::NotUtf8
            | PathError::NotPlain { .. }
            | PathError::SymlinkComponent { .. }
            | PathError::Unverifiable { .. }
            | PathError::Fs(_) => {}
        }
    }
    shapes
}

#[test]
fn a7_table_is_total_over_path_error() {
    for err in every_shape() {
        let code = code_for(&err);
        let finding = path_finding(&err);
        assert_eq!(code.is_some(), finding.is_some(), "{err:?}");
        let text = err.to_string();
        assert!(!text.is_empty(), "{err:?} renders nothing");
        match &err {
            PathError::SymlinkComponent { .. } | PathError::Unverifiable { .. } => {
                let finding = finding.unwrap();
                assert_eq!(code, Some(codes::SYMLINKED_CONTENT_REJECTED));
                assert_eq!(finding.code, code);
                assert_eq!(finding.message, text, "one sentence, two surfaces");
                assert!(finding.source.is_some(), "attributed to the rejected key");
            }
            PathError::Escapes { authored } => {
                assert_eq!(
                    text,
                    format!("`{authored}` resolves outside the workspace root")
                );
            }
            // The read's own sentence (`fs::plain`), one step earlier.
            PathError::NotPlain { component } => {
                assert_eq!(
                    text,
                    format!(
                        "`{component}` is not a plain path component (a name bears no `\\`) — \
                         rename it with a plain name"
                    )
                );
            }
            _ => assert!(code.is_none()),
        }
    }
}

#[test]
fn symlink_finding_names_component_never_target() {
    let a = PathError::SymlinkComponent {
        component: "lib".into(),
        key: key("tenants/lib/x.nml"),
    };
    let d = path_finding(&a).unwrap();
    assert_eq!(
        d.message,
        "closed binding rejects `tenants/lib/x.nml`: path component `lib` is a symlink — \
         content reached through a symlinked path is rejected in a closed universe (a link \
         could relocate content into a differently-trusted subtree); replace the link with \
         the content itself, or check the file at its real path"
    );
    assert_eq!(d.source.as_deref(), Some("tenants/lib/x.nml"));
    let f2 = path_finding(&PathError::Unverifiable {
        key: key("ADMIN/s.nml"),
    })
    .unwrap();
    assert_eq!(
        f2.message,
        "closed binding rejects `ADMIN/s.nml`: cannot verify the on-disk spelling of this \
         path on this backend — spell the path exactly as the filesystem does"
    );
}

#[test]
fn unit_truncation_names_the_unit_the_stop_and_the_blast_radius() {
    // A16 amendment (r73): the NML2089 sentence for a spent BUDGET UNIT
    // — the DEFAULT lane's proof of the text the perf-tier drive prints
    // at the real bound. It is attributed to the KEY it denies (a
    // per-file finding, the shape rule-3 ambiguity uses), names the
    // unit, the stop directory and the constant, and says plainly what
    // is NOT affected. It offers no flag: `--root` is the CLI's, and it
    // is not even the remedy here (a root inside the unit leaves the
    // operator's manifest outside the universe, re-opening it).
    let truncation = UnitTruncation {
        unit: key("tenants/cu"),
        stop: key("tenants/cu/spam"),
        why: UnitBound::Entries,
    };
    let d = crate::workspace::diag::unit_truncated(&key("tenants/cu/plain.flow.nml"), &truncation);
    assert_eq!(d.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(d.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(d.source.as_deref(), Some("tenants/cu/plain.flow.nml"));
    assert_eq!(
        d.message,
        "the discovery budget for `tenants/cu` is exhausted: the walk stopped at \
         `tenants/cu/spam` (the 65536-entry bound for this subtree was reached) — every file \
         under `tenants/cu` is denied and validates under no binding; files outside \
         `tenants/cu` are unaffected; reduce the number of entries under `tenants/cu`"
    );
    assert!(!d.message.contains("--root"), "{}", d.message);
    // The whole-universe row keeps its own sentence, and its own
    // attribution (a DIRECTORY, and every file under the root).
    let whole = crate::workspace::diag::universe_truncated(&Truncation::Entries {
        dir: key("admin/spam"),
    });
    assert_eq!(whole.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(whole.source.as_deref(), Some("admin/spam"));
    assert!(
        whole
            .message
            .starts_with("cannot enumerate manifests: the walk stopped at `"),
        "{}",
        whole.message
    );
}

#[test]
fn unit_truncation_sentences_name_the_bound_that_was_spent() {
    // r75: the unit's byte budget names the INPUT it stopped at and its
    // remedy is smaller live inputs; an unlistable directory names the
    // directory and asks for it to be made readable. Neither offers a
    // flag, exactly like the entry-bound sentence above.
    let bytes = UnitTruncation {
        unit: key("tenants/cu"),
        stop: key("tenants/cu/other/v03/core.model.nml"),
        why: UnitBound::LiveInputBytes,
    };
    let d = crate::workspace::diag::unit_truncated(&key("tenants/cu/flows/a.flow.nml"), &bytes);
    assert_eq!(d.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(d.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(d.source.as_deref(), Some("tenants/cu/flows/a.flow.nml"));
    assert_eq!(
        d.message,
        "the discovery budget for `tenants/cu` is exhausted: the walk stopped at \
         `tenants/cu/other/v03/core.model.nml` (the 67108864-byte live-input budget for this \
         subtree was spent reading it) — every file under `tenants/cu` is denied and validates \
         under no binding; files outside `tenants/cu` are unaffected; use fewer or smaller live \
         manifests, project configs and declared sources under `tenants/cu`"
    );
    let unreadable = UnitTruncation {
        unit: key("tenants/cu"),
        stop: key("tenants/cu/locked"),
        why: UnitBound::Unreadable(FsError::Denied),
    };
    let d = crate::workspace::diag::unit_truncated(&key("tenants/cu/plain.flow.nml"), &unreadable);
    assert_eq!(d.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(
        d.message,
        "discovery under `tenants/cu` was cut short: the walk stopped at `tenants/cu/locked` \
         (unreadable: permission denied on a path component) — every file under `tenants/cu` \
         is denied and validates under no binding; files outside `tenants/cu` are unaffected; \
         make `tenants/cu/locked` readable"
    );
    assert!(!d.message.contains("--root"), "{}", d.message);
}

#[test]
fn the_universe_byte_budget_sentence_names_the_input_and_its_own_remedy() {
    // r75 (r74-kernel F7): the whole-universe byte truncation names the
    // input that crossed the bound and ends in the kernel's own remedy —
    // smaller live inputs, not a smaller tree — so the CLI has no
    // `--root` sentence to append to it.
    let d = crate::workspace::diag::universe_truncated(&Truncation::LiveInputBytes {
        key: key("vendor/v8/core.model.nml"),
    });
    assert_eq!(d.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(d.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(d.source.as_deref(), Some("vendor/v8/core.model.nml"));
    assert_eq!(
        d.message,
        "cannot enumerate manifests: the live-input budget (67108864 bytes) was spent \
         reading `vendor/v8/core.model.nml` — the universe is treated as closed and no binding \
         governs any file; use fewer or smaller live manifests, project configs and declared \
         sources under the root"
    );
}

#[test]
fn the_universe_byte_backstop_sentence_names_the_total_and_the_input() {
    // r77 (r76 F1 (b)): the universe-wide byte backstop is its own
    // sentence — the total, every unit summed, and the input that
    // crossed it — ending in the kernel's remedy. Unlike the root
    // unit's byte sentence, the CLI appends its `--root` advice to
    // this one: for sixteen units' worth of live inputs a smaller tree
    // IS a remedy (the entry backstop's doctrine).
    let d = crate::workspace::diag::universe_truncated(&Truncation::TotalLiveInputBytes {
        key: key("tenants/t15/other/v15/core.model.nml"),
    });
    assert_eq!(d.severity, nml_core::diagnostic::Severity::Error);
    assert_eq!(d.code, Some(codes::UNIVERSE_TRUNCATED));
    assert_eq!(
        d.source.as_deref(),
        Some("tenants/t15/other/v15/core.model.nml")
    );
    assert_eq!(
        d.message,
        format!(
            "cannot enumerate manifests: the universe-wide live-input budget \
             ({MAX_TOTAL_LIVE_INPUT_BYTES} bytes, every budget unit summed) was spent reading \
             `tenants/t15/other/v15/core.model.nml` — the universe is treated as closed and no \
             binding governs any file; use fewer or smaller live manifests, project configs and \
             declared sources across the tree"
        )
    );
    assert!(d.message.contains("(1073741824 bytes,"), "{}", d.message);
}

/// RFC 0026 decision 3: a failed manifest's first finding rides the
/// NML2088 row with its remedy — the did-you-mean in the manifest's
/// FILE (`source` = the key), stamped explicitly, since the editor
/// shows the row on the files the manifest would govern; the cause's
/// place is the manifest's too.
#[test]
fn a_failed_manifests_remedy_rides_its_row_in_the_manifests_file() {
    let span = nml_core::span::Span::new(24, 30);
    let finding = nml_core::diagnostic::Diagnostic::error("unknown property 'versio'")
        .with_code(codes::UNKNOWN_PROPERTY)
        .with_span(span)
        .with_suggestion(Suggestion::did_you_mean("version").at(span));
    let err = crate::package::PackageError::Manifest {
        errors: vec![finding],
        at: None,
    };
    let row = crate::workspace::diag::manifest_unloadable(&key("demo.package.nml"), &err);
    assert_eq!(row.code, Some(codes::RESOLUTION_INPUT_UNLOADABLE));
    assert_eq!(
        row.suggestions,
        vec![
            Suggestion::did_you_mean("version")
                .at(span)
                .in_file("demo.package.nml")
        ],
        "{row:?}"
    );
    assert_eq!(
        row.suggestion_source(&row.suggestions[0]),
        Some("demo.package.nml")
    );
    assert_eq!(row.cause_source(), Some("demo.package.nml"));
    assert!(
        row.rendered_message()
            .ends_with("unknown property 'versio' (did you mean \"version\"?)"),
        "{}",
        row.rendered_message()
    );
}

/// Every kernel error is `std::error::Error`, so an embedder can `?` a
/// resolution failure straight into `Box<dyn Error>` — `PathError` used
/// to be the one that could not (its sentence lived in a free
/// `diag::describe`, so `resolve_file(..)?` did not compile in a `fn
/// main() -> Result<(), Box<dyn Error>>`), and its `Display` must still
/// be the A7 sentence, byte for byte.
#[test]
fn path_error_is_a_std_error_with_the_a7_sentence() {
    fn boxed(err: PathError) -> Box<dyn std::error::Error> {
        Box::new(err)
    }
    fn propagates(err: PathError) -> Result<(), Box<dyn std::error::Error>> {
        Err(err)?;
        Ok(())
    }
    for shape in every_shape() {
        let sentence = shape.to_string();
        assert!(!sentence.is_empty(), "{shape:?} has no sentence");
        assert_eq!(boxed(shape.clone()).to_string(), sentence);
        assert_eq!(propagates(shape.clone()).unwrap_err().to_string(), sentence);
    }
    // Byte for byte: the six sentences the deleted `diag::describe`
    // spelled, transcribed from its arms. A `Display` impl rewraps its
    // literals, and a rewrap that moves ONE space is a wording
    // regression no golden covers — every one of these was a free
    // function's `format!` the day before.
    assert_eq!(
        PathError::NotRelative.to_string(),
        "not a relative path (no scheme, no leading separator, no drive prefix; must name a file)"
    );
    assert_eq!(
        PathError::Escapes {
            authored: "a/../../b.nml".into()
        }
        .to_string(),
        "`a/../../b.nml` resolves outside the workspace root"
    );
    assert_eq!(
        PathError::Depth.to_string(),
        format!(
            "more than {} path components — nothing this deep is keyable; flatten the tree, or \
             move the file where the walk lists it",
            crate::workspace::paths::MAX_COMPONENTS
        )
    );
    assert_eq!(
        PathError::NotUtf8.to_string(),
        "a path component is not UTF-8 — rename it with a plain name"
    );
    assert_eq!(
        PathError::NotPlain {
            component: "ev\\il".to_string()
        }
        .to_string(),
        "`ev\\il` is not a plain path component (a name bears no `\\`) — rename it with a \
         plain name"
    );
    assert_eq!(
        PathError::Fs(FsError::Denied).to_string(),
        FsError::Denied.to_string()
    );
    // And the two arms that ARE the diagnostic's sentence stay the
    // diagnostic's, not a second spelling of it.
    for shape in every_shape() {
        if matches!(
            shape,
            PathError::SymlinkComponent { .. } | PathError::Unverifiable { .. }
        ) {
            assert_eq!(
                shape.to_string(),
                path_finding(&shape).expect("a finding").message,
                "{shape:?}"
            );
        }
    }
}

/// r103-cov: the gate's row table is TOTAL over `Skip`, and the two
/// reasons that are NOT findings stay that way.
///
/// `diag::skipped` is where the walk's policy words become the gate's
/// rows. Its `None` arms are a contract: a dot-directory is audited and
/// reported through `skipped_under` (one row per directory, never two),
/// and a build product (`node_modules`, `target`) is a closing-row fact
/// only — a repository with a `node_modules/x.nml` must not fail
/// `nml check .`. Nothing pinned the `None` half: turning a policy
/// directory into a finding left every test green, and every CI run over
/// a repository with dependencies installed would have failed.
#[test]
fn the_gates_row_table_is_total_over_skip_and_its_silent_reasons_stay_silent() {
    use crate::workspace::diag::skipped;
    use crate::workspace::{EntryKind, Skip, Skipped};
    use nml_core::diagnostic::Severity;
    let at = |s: &str| SourceKey::checked(s).expect(s);
    let row = |why: Skip, closed: bool| {
        skipped(
            &Skipped {
                key: at("tenants/cu/thing.nml"),
                why,
            },
            closed,
        )
    };
    // The reasons that ARE rows, with the severity each carries.
    for (why, severity) in [
        (Skip::Fifo, Severity::Error),
        (Skip::DotFile, Severity::Error),
        (Skip::ComponentBound, Severity::Error),
        (
            Skip::UnkeyableName {
                kind: EntryKind::Dir,
                name: "ev\\il".to_string(),
            },
            Severity::Error,
        ),
        (Skip::Symlink, Severity::Error),
    ] {
        let d = row(why.clone(), false).unwrap_or_else(|| panic!("{why:?} has no row"));
        assert_eq!(d.severity, severity, "{why:?}: {d:?}");
        assert!(d.code.is_some(), "{why:?}: {d:?}");
        assert!(d.source.is_some(), "{why:?} names its key: {d:?}");
    }
    // A symlink whose name is not `.nml`-shaped is the one WARNING (what
    // lies beneath it is exactly what the walk never learns).
    let link = skipped(
        &Skipped {
            key: at("tenants/cu/lib"),
            why: Skip::Symlink,
        },
        false,
    )
    .expect("a row");
    assert_eq!(link.severity, Severity::Warning, "{link:?}");
    // And the two that are NEVER findings, in either universe.
    for why in [Skip::DotDirectory, Skip::PolicyDirectory] {
        for closed in [false, true] {
            assert!(
                row(why.clone(), closed).is_none(),
                "{why:?} (closed={closed}) must not be a gate finding"
            );
        }
    }
}
