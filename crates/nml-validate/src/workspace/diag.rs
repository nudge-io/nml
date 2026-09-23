//! The A7 table (RFC 0019 item 0): typed kernel outcomes → stable codes,
//! total over [`PathError`] — and over [`Skip`], the gate's rows. The
//! only place that mints the workspace codes (NML2080, NML2083,
//! NML2087–NML2091), so a new variant cannot ship without a sentence —
//! every match is exhaustive and `a7_table_is_total_over_path_error`
//! walks the path one.

use nml_core::diagnostic::{Code, Diagnostic, codes};
use nml_core::span::Span;

use crate::package::PackageError;

use super::claims::{Claimant, UnitBound, UnitTruncation};
use super::discover::{
    HiddenAudit, MAX_ENTRIES, MAX_LIVE_INPUT_BYTES, MAX_TOTAL_ENTRIES, MAX_TOTAL_LIVE_INPUT_BYTES,
    Skip, Skipped, Truncation,
};
use super::paths::{MAX_COMPONENTS, PathError, SourceKey};
use crate::file_names::is_nml_name;
use crate::fs::EntryKind;
use crate::glob::UnitGap;

/// The stable code a kernel error diagnoses under, when it is a
/// diagnostic at all. `None` rows are CLI/embedder errors (the root is an
/// operator input) or RFC 0020's (NML2069 is allocated there).
pub(crate) fn code_for(err: &PathError) -> Option<Code> {
    match err {
        PathError::SymlinkComponent { .. } | PathError::Unverifiable { .. } => {
            Some(codes::SYMLINKED_CONTENT_REJECTED)
        }
        PathError::NotRelative
        | PathError::Escapes { .. }
        | PathError::Depth
        | PathError::NotUtf8
        | PathError::NotPlain { .. }
        | PathError::Fs(_) => None,
    }
}

/// The diagnostic for a kernel error that IS one (NML2083, two forms).
/// The message never names a symlink's target — the kernel never
/// resolved it — so it is byte-identical whether or not the target
/// exists (E26).
pub(crate) fn path_finding(err: &PathError) -> Option<Diagnostic> {
    path_finding_typed(err, None)
}

/// The NML2083 finding with the rejected path named AS TYPED beside its key
/// when the two spellings differ: ``closed binding rejects `<typed>` (key
/// `<key>`): …``. A spelling through `..` names a component the key no
/// longer carries (`lib` in `tenants/cu/lib/../plain.flow.nml`), so a
/// front end that took the spelling from its user hands it back here —
/// the sentence has one owner. `None`, or a spelling equal to the key,
/// renders the key alone.
pub fn path_finding_typed(err: &PathError, typed: Option<&str>) -> Option<Diagnostic> {
    let code = code_for(err)?;
    let subject = |key: &SourceKey| match typed {
        Some(typed) if typed != key.as_str() => format!("`{typed}` (key `{key}`)"),
        _ => format!("`{key}`"),
    };
    let (key, message) = match err {
        PathError::SymlinkComponent { component, key } => (
            key,
            format!(
                "closed binding rejects {}: path component `{component}` is a \
                     symlink — content reached through a symlinked path is rejected in a \
                     closed universe (a link could relocate content into a \
                     differently-trusted subtree); replace the link with the content \
                     itself, or check the file at its real path",
                subject(key)
            ),
        ),
        PathError::Unverifiable { key } => (
            key,
            format!(
                "closed binding rejects {}: cannot verify the on-disk spelling of \
                 this path on this backend — spell the path exactly as the filesystem \
                 does",
                subject(key)
            ),
        ),
        _ => return None,
    };
    Some(
        Diagnostic::error(message)
            .with_code(code)
            .with_source(key.as_str().to_string()),
    )
}

/// The CLI/embedder message for a kernel error that is not a diagnostic
/// (or the diagnostic's own sentence). `Escapes` prints the AUTHORED path
/// only — never a resolved target. The impl lives beside the A7 table
/// rather than beside the type because two arms ARE the diagnostic's
/// sentence (`path_finding`); [`PathError`] is `std::error::Error` like
/// every other kernel error ([`RootError`](super::paths::RootError),
/// [`FsError`](crate::fs::FsError), [`OpenError`](crate::fs::OpenError)),
/// so an embedder can `?` a resolution failure into `Box<dyn Error>`.
impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::NotRelative => f.write_str(
                "not a relative path (no scheme, no leading separator, no drive prefix; must \
                 name a file)",
            ),
            PathError::Escapes { authored } => {
                write!(f, "`{authored}` resolves outside the workspace root")
            }
            PathError::Depth => write!(
                f,
                "more than {} path components — nothing this deep is keyable; flatten the tree, \
                 or move the file where the walk lists it",
                super::paths::MAX_COMPONENTS
            ),
            PathError::NotUtf8 => {
                f.write_str("a path component is not UTF-8 — rename it with a plain name")
            }
            // The read's own sentence (`fs::plain`), one step earlier, with the
            // remedy a typed target's author can act on.
            PathError::NotPlain { component } => write!(
                f,
                "`{component}` is not a plain path component (a name bears no `\\`) — rename it \
                 with a plain name"
            ),
            PathError::SymlinkComponent { .. } | PathError::Unverifiable { .. } => {
                f.write_str(&path_finding(self).map(|d| d.message).unwrap_or_default())
            }
            PathError::Fs(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PathError {}

/// NML2080: a resolution input at `key` is inert — inside content the
/// live outer binding `claimant` claims.
pub(crate) fn inert_input(key: &SourceKey, what: &str, claimant: &Claimant<'_>) -> Diagnostic {
    Diagnostic::warning(format!(
        "{what} `{key}` is inert: it sits inside content claimed by binding '{}' of {} \
         (files[{}] = {:?}) — content, not configuration; its pins, autoAssociate, \
         bindings and anchoring are ignored",
        claimant.binding.name,
        claimant.claim.manifest_label(),
        claimant.glob,
        claimant.binding.files[claimant.glob],
    ))
    .with_code(codes::INERT_RESOLUTION_INPUT)
    .with_source(key.as_str().to_string())
}

/// NML2080: a root marker nested under a live marker of the same package.
pub(crate) fn nested_marker(key: &SourceKey, package: &str, outer: &SourceKey) -> Diagnostic {
    Diagnostic::warning(format!(
        "root marker `{key}` is inert: nested under the live `{}` marker of package '{package}' \
         at `{}` — a marker inside a marked subtree is content, not an anchor",
        key.file_name(),
        outer.dir(),
    ))
    .with_code(codes::INERT_RESOLUTION_INPUT)
    .with_source(key.as_str().to_string())
}

/// NML2089 (A16): the walk was cut short — the entry bound was reached,
/// a directory could not be listed, or a byte budget was spent — so the
/// universe is closed-denied in full. Attributed to the directory where
/// the walk stopped (the root directory spells `.`). The FACT only —
/// where, why, and that nothing binds; what to do about it ("pass
/// `--root` to a smaller tree") is the CLI reporter's sentence, not
/// Layer B's (E35): the editor has no flag to offer. The root itself is
/// the run's fact, stated once per front end, never in this sentence.
/// Minted here, once.
pub(crate) fn universe_truncated(truncation: &Truncation) -> Diagnostic {
    let (dir, why) = match truncation {
        Truncation::Entries { dir } => (dir, format!("the {MAX_ENTRIES}-entry bound was reached")),
        Truncation::Unreadable { dir, error } => (dir, format!("unreadable: {error}")),
        // The byte budget names an INPUT, not a directory, and its
        // remedy is smaller live inputs, not a smaller tree — its own
        // sentence, ending in the one remedy the kernel can name,
        // so the CLI appends no `--root` advice to it.
        Truncation::LiveInputBytes { key } => {
            return Diagnostic::error(format!(
                "cannot enumerate manifests: the live-input budget ({MAX_LIVE_INPUT_BYTES} \
                 bytes) was spent reading `{key}` — the universe is treated as closed and no \
                 binding governs any file; use fewer or smaller live manifests, project configs \
                 and declared sources under the root"
            ))
            .with_code(codes::UNIVERSE_TRUNCATED)
            .with_source(key.as_str().to_string());
        }
        // The universe-wide byte backstop names the input that
        // crossed it. Its remedy is fewer live inputs across the tree
        // — or, as for the entry backstop (the sixteen-tenant
        // doctrine), a smaller tree: the CLI's sentence to add.
        Truncation::TotalLiveInputBytes { key } => {
            return Diagnostic::error(format!(
                "cannot enumerate manifests: the universe-wide live-input budget \
                 ({MAX_TOTAL_LIVE_INPUT_BYTES} bytes, every budget unit summed) was spent \
                 reading `{key}` — the universe is treated as closed and no binding governs \
                 any file; use fewer or smaller live manifests, project configs and declared \
                 sources across the tree"
            ))
            .with_code(codes::UNIVERSE_TRUNCATED)
            .with_source(key.as_str().to_string());
        }
    };
    let at = dir.dir_label();
    // The remedy that is no front end's flag — remove the flood, make
    // the directory readable — is the kernel's to say: the editor
    // showed this row with no next step at all, while the CLI appended
    // its `--root` clause after it.
    Diagnostic::error(format!(
        "cannot enumerate manifests: the walk stopped at `{at}` ({why}) — the universe is \
         treated as closed and no binding governs any file; remove what stopped the walk"
    ))
    .with_code(codes::UNIVERSE_TRUNCATED)
    .with_source(at.to_string())
}

/// NML2089 (A16 amendment): the BUDGET UNIT
/// rooted at `truncation.unit` was cut short at `truncation.stop` — the
/// entry bound reached while listing it, the unit's live-input budget
/// spent reading it, or the directory unlistable — so every key under
/// the unit is denied — and only those. Attributed to the KEY it
/// denies, not to a directory: this is a per-file finding in the shape
/// rule-3 ambiguity uses, never a universe error, because a file outside
/// the unit is unaffected. The FACT and the one remedy that is not a
/// front end's flag; `--root` is NOT the remedy (rooting inside the unit
/// leaves the operator's manifest outside the universe, which re-opens
/// it).
pub(crate) fn unit_truncated(key: &SourceKey, truncation: &UnitTruncation) -> Diagnostic {
    let UnitTruncation { unit, stop, why } = truncation;
    let (lead, cut, remedy) = match why {
        UnitBound::Entries => (
            format!("the discovery budget for `{unit}` is exhausted"),
            format!(
                "the walk stopped at `{stop}` (the {MAX_ENTRIES}-entry bound for this subtree \
                 was reached)"
            ),
            format!("reduce the number of entries under `{unit}`"),
        ),
        UnitBound::LiveInputBytes => (
            format!("the discovery budget for `{unit}` is exhausted"),
            format!(
                "the walk stopped at `{stop}` (the {MAX_LIVE_INPUT_BYTES}-byte live-input \
                 budget for this subtree was spent reading it)"
            ),
            format!(
                "use fewer or smaller live manifests, project configs and declared sources \
                 under `{unit}`"
            ),
        ),
        UnitBound::Unreadable(error) => (
            format!("discovery under `{unit}` was cut short"),
            format!("the walk stopped at `{stop}` (unreadable: {error})"),
            format!("make `{stop}` readable"),
        ),
    };
    // One sentence in the voice every other row speaks: lowercase
    // clauses joined by `;`, no capital and no full stop — the unit row
    // was the only diagnostic in the toolkit that ended in a period.
    Diagnostic::error(format!(
        "{lead}: {cut} — every file under `{unit}` is denied and validates under no binding; \
         files outside `{unit}` are unaffected; {remedy}"
    ))
    .with_code(codes::UNIVERSE_TRUNCATED)
    .with_source(key.as_str().to_string())
}

/// RFC 0019 E38 (item 4): NML2092 for every binding glob of `manifest`
/// that delegates shallower than the unit inferred from its last wildcard
/// run ([`crate::glob::unit_gap`]) — so the content between is the root
/// unit's, where one tenant's flood denies everyone — under INFERENCE only: an explicit
/// `budgetUnits` declaration replaces the inference and silences it.
/// Spanned at the glob; one sentence for both front ends. RFC 0026 B-3: a
/// gap whose inferred unit nests inside another glob's the multiplying way
/// ([`crate::glob::unit_nests_inside`]) — always a gap, by construction:
/// the inner glob's last wildcard run starts past a literal that follows
/// the outer unit's wildcard — takes the NESTED form, naming the outer
/// unit and the one declaration the loader accepts.
///
/// It lives here, beside the row it mints, and not on the manifest: a
/// WORKSPACE finding is the workspace's to compute. The loader reached up
/// into `workspace::diag` for the row while `workspace` read the loader —
/// a dependency cycle whose only content was this pass.
pub(crate) fn budget_unit_gaps(manifest: &crate::package::PackageManifest) -> Vec<Diagnostic> {
    if !manifest.budget_units.is_empty() {
        return Vec::new();
    }
    let units: Vec<(&str, usize, &str, String)> = manifest
        .validators
        .iter()
        .flat_map(|binding| {
            binding
                .files
                .iter()
                .enumerate()
                .filter_map(move |(index, glob)| {
                    crate::glob::inferred_unit(glob)
                        .map(|unit| (binding.name.as_str(), index, glob.as_str(), unit))
                })
        })
        .collect();
    let mut gaps = Vec::new();
    for binding in &manifest.validators {
        for (index, (glob, span)) in binding.files.iter().zip(&binding.file_spans).enumerate() {
            if let Some(gap) = crate::glob::unit_gap(glob) {
                let outer = units
                    .iter()
                    .find(|(b, i, _, unit)| {
                        (*b != binding.name || *i != index)
                            && crate::glob::unit_nests_inside(unit, &gap.inferred)
                    })
                    .map(|(b, i, g, unit)| (*b, *i, *g, unit.as_str()));
                gaps.push(budget_unit_gap(
                    &binding.name,
                    index,
                    glob,
                    &gap,
                    outer,
                    *span,
                ));
            }
        }
    }
    gaps
}

/// WARNING on the manifest, at the glob (the editor squiggles it, the
/// CLI prints it once per run); silenced by an explicit `budgetUnits`,
/// which the sentence spells both ways. The FACT and the remedy the
/// kernel can name; minted here, once.
///
/// `outer` (RFC 0026 B-3) is another glob's inferred unit the gap's
/// inferred unit nests inside the multiplying way — `(binding, files
/// index, glob, unit)` — and turns the remedy into the ONE declaration
/// the loader accepts: the delegated subtree, never the inferred
/// boundary (declared beside the outer unit it would be refused).
pub(crate) fn budget_unit_gap(
    binding: &str,
    index: usize,
    glob: &str,
    gap: &UnitGap,
    outer: Option<(&str, usize, &str, &str)>,
    span: Span,
) -> Diagnostic {
    let UnitGap {
        delegated,
        inferred,
        unbounded,
    } = gap;
    let message = if *unbounded {
        format!(
            "binding '{binding}' files[{index}] = {glob:?}: its wildcard directories have no \
             fixed depth above the inferred budget unit `{inferred}`, so the content between \
             them stays in the root unit, where one tenant's flood denies everyone; spell the \
             layout with a fixed depth (`*`, not `**`) above its unit boundary, then declare \
             `budgetUnits` for it"
        )
    } else if let Some((outer_binding, outer_index, outer_glob, outer_unit)) = outer {
        format!(
            "binding '{binding}' files[{index}] = {glob:?}: the inferred budget unit is \
             `{inferred}` — content under `{delegated}` outside it stays in the root unit, \
             where one tenant's flood denies everyone, and the unit nests inside \
             `{outer_unit}` (binding '{outer_binding}' files[{outer_index}] = {outer_glob:?}), \
             where every directory that unit delegates would mint units of its own beneath \
             it; declare budgetUnits = [{delegated:?}] to keep one unit per delegated subtree \
             (the inferred boundary cannot be declared beside it)"
        )
    } else {
        format!(
            "binding '{binding}' files[{index}] = {glob:?}: the inferred budget unit is \
             `{inferred}` — content under `{delegated}` outside it stays in the root unit, \
             where one tenant's flood denies everyone; declare budgetUnits = [{delegated:?}] to \
             isolate each delegated subtree, or [{inferred:?}] to keep the inferred boundary"
        )
    };
    Diagnostic::warning(message)
        .with_code(codes::BUDGET_UNIT_GAP)
        .with_span(span)
}

/// NML2088 (E27(3)): a LIVE resolution input at `key` — `what` is
/// `manifest` or `project config` — could not be loaded, for `why` (the
/// reader's refusal, a malformed text, a declared source unavailable, a
/// stem that contradicts the declared name). It contributes no claim and
/// CLOSES the universe: a malformed operator input must never reopen a
/// repo into the permissive default. Minted here, once.
pub(crate) fn input_unloadable(key: &SourceKey, what: &str, why: &str) -> Diagnostic {
    Diagnostic::error(format!("{what} failed to load: {why}"))
        .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
        .with_source(key.as_str().to_string())
}

/// The row for a live manifest that failed to load: NML2088 — or, for a
/// manifest whose `layers:` grant breaks a rule of the loader's own, that
/// rule's code (NML2081; NML2082 for a `[]directive` entry that
/// redeclares one of the language's merge-policy directives), so a
/// consumer filtering by code lands on it.
/// One rule: a loader finding with a code of its own is reported under
/// it; the manifest's other failures (a parse error, a meta-validation
/// finding, a declared source, a loader rule under its own code) are the
/// universe's NML2088. A manifest finding — a parse error, the
/// meta-schema's, the loader's — locates the row AT the finding: its
/// span, in the manifest's text, is the row's own (the CLI prints
/// `key:line:col:` and fills `line`/`col` on the wire, the editor
/// squiggles it), stated ONCE — the sentence names no line. Either way
/// the universe is closed-denied around the manifest. The first finding's
/// notes ride the row, stamped with the manifest's key — the first
/// `files` of a repeated entry (NML2093), located in the manifest — since
/// the CLI never judges a manifest under an unloadable universe: this row
/// is where a reader sees both places. The finding's remedies ride the
/// row too, in the manifest's file (`suggestions[]` with `source`): the
/// CLI's line carries the did-you-mean, `nml fix` reports the edit as
/// pending in the manifest (the door rewrites nothing), the editor
/// offers the quick fix on the manifest from a governed file's row.
/// The finding itself rides the
/// row as its cause ([`Diagnostic::caused_by`]) — its code, sentence
/// and place as facts on the wire — where it has a code of its own
/// and the row is not reported under it — every finding the loader
/// states has one (NML2094–NML2104 for its own rules), and the
/// formatVersion gate, stated as values, contributes its coded finding
/// ([`PackageError::gate_finding`]) beneath the sentence the row
/// carried before.
pub(crate) fn manifest_unloadable(key: &SourceKey, err: &PackageError) -> Diagnostic {
    let first = match err {
        PackageError::Manifest { errors, .. } => errors.first().map(|first| (first, errors.len())),
        _ => None,
    };
    match first {
        Some((finding, count)) => {
            // The loader's rules whose code IS the verdict ride it — the
            // grant's (NML2081) and the reserved-directive rule (NML2082),
            // one class, one arm — so a consumer filtering by code lands
            // on the row itself; such a row carries no cause (`caused_by`).
            let code = match finding.code {
                Some(code @ (codes::LAYER_GRANT_RULE | codes::RESERVED_DIRECTIVE)) => code,
                _ => codes::RESOLUTION_INPUT_UNLOADABLE,
            };
            // ONE verdict per row: the wrapper already says the manifest
            // did not load, so repeating the library's own
            // "manifest failed validation" stuttered the same fact twice
            // before the reader reached the finding that explains it.
            let row = Diagnostic::error(format!(
                "manifest failed to load{}: {}",
                crate::package::finding_of(count),
                finding.message
            ))
            .with_code(code)
            .with_source(key.as_str().to_string());
            let mut row = match finding.span {
                Some(span) => row.with_span(span),
                None => row,
            };
            // The first finding's notes ride the row — the first `files` of
            // a repeated entry (NML2093), located in the manifest — stamped
            // with the manifest's key, as NML2091 stamps its source's line:
            // the editor's degraded note keeps no row source, and an
            // unstamped note would map into the DOCUMENT the row sits on.
            for note in &finding.related {
                row = row.with_related_in(
                    note.span,
                    note.message.clone(),
                    Some(key.as_str().to_string()),
                );
            }
            // The carried remedies are stamped with the row's own source —
            // the key — by the one rule (`caused_by`'s last fallback).
            row.caused_by(finding, None)
        }
        None => {
            let row = input_unloadable(key, "manifest", &err.to_string());
            match err.gate_finding() {
                Some(gate) => row.caused_by(&gate, None),
                None => row,
            }
        }
    }
}

/// NML2091: the binding that governs `key` cannot build its validator —
/// the package layer's `err` ([`PackageError::Sources`]: a declared
/// source fails to load). The row sits on the FILE (the binding's every
/// file gets one), names the binding, its manifest and the first failing
/// finding WHERE IT SITS — `<source key>:<line>:<col>` for a workspace
/// manifest's source, the logical name for an external package's — and
/// carries that finding as a related note in the source's own file, so
/// an editor can jump to it — and as the row's cause
/// ([`Diagnostic::caused_by`]): its code, sentence and place as facts
/// on the wire, in the source's own file, its remedies riding the row
/// in that file — the source's KEY, the spelling the note, the cause and
/// the remedy share (a logical name for an external package's source
/// names no file, and no applier resolves such an edit). The file validates under no
/// binding at all (never a registry, never parse-only). Minted here,
/// once.
pub(crate) fn validator_unbuildable(
    key: &SourceKey,
    claimant: &Claimant<'_>,
    err: &PackageError,
) -> Diagnostic {
    let claim = claimant.claim;
    let head = format!(
        "binding '{}' of {} cannot build its validator",
        claimant.binding.name, claim.manifest_label
    );
    let tail = "the file validates under no binding until the source loads";
    let PackageError::Sources { errors } = err else {
        return Diagnostic::error(format!("{head}: {err} — {tail}"))
            .with_code(codes::VALIDATOR_UNBUILDABLE)
            .with_source(key.as_str().to_string());
    };
    let Some(first) = errors.first() else {
        return Diagnostic::error(format!("{head}: schema sources failed to load — {tail}"))
            .with_code(codes::VALIDATOR_UNBUILDABLE)
            .with_source(key.as_str().to_string());
    };
    // The failing source: its logical name (what the loader attributes
    // findings to), its declared file, and — for a workspace manifest —
    // its key, the spelling a note is located in.
    let logical = first.source.as_deref().unwrap_or("?");
    let entry = claim
        .package
        .manifest
        .schemas
        .iter()
        .find(|s| s.name == logical);
    let source_key = claim
        .manifest()
        .zip(entry)
        .map(|(manifest, entry)| manifest.dir().join(&entry.file));
    let text = claim
        .package
        .sources
        .iter()
        .find(|(name, _)| name == logical)
        .map(|(_, text)| text);
    let location = match (first.span, text) {
        (Some(span), Some(text)) => {
            let loc = nml_core::span::SourceMap::new(text).location(span.start);
            format!(":{}:{}", loc.line, loc.column)
        }
        _ => String::new(),
    };
    let name = source_key
        .as_ref()
        .map_or_else(|| logical.to_string(), |k| k.to_string());
    let more = crate::package::finding_of(errors.len());
    let mut diag = Diagnostic::error(format!(
        "{head}: declared source `{logical}` failed to load at {name}{location}{more}: {} — {tail}",
        first.message
    ))
    .with_code(codes::VALIDATOR_UNBUILDABLE)
    .with_source(key.as_str().to_string());
    if let (Some(span), Some(source_key)) = (first.span, source_key) {
        diag = diag.with_related_in(span, first.message.clone(), Some(source_key.to_string()));
    }
    // The loader attributes a source's findings to its LOGICAL name
    // (`load_schema_parts`); this row's consumers read files by KEY. The
    // wrapped finding is re-spelled in the row's own vocabulary — the
    // source's key, else its logical name (an external package's source
    // names no file, and no applier resolves such an edit) — BEFORE it
    // is wrapped, so its cause and its carried remedies name the one
    // file the note above names: a remedy stamped `core` reached no
    // applier (the CLI's foreign read dropped it from the wire in
    // silence; the editor's quick fix targeted a file that does not
    // exist).
    let first = first.clone().with_source(name.clone());
    diag.caused_by(&first, Some(name))
}

/// The claimants an ambiguous claim names — ONE rendering, shared by the
/// error finding and `nml binding`'s `AMBIGUOUS` row: `<n> manifests
/// claim this file: <manifest> (<binding>, files[<i>] = "<glob>"), …`,
/// in manifest-label order (then binding name, then glob index), so the
/// text never depends on the order the candidates were collected in.
pub fn ambiguous_claim_summary(claimants: &[Claimant<'_>]) -> String {
    let mut parts: Vec<(&str, &str, usize, &str)> = claimants
        .iter()
        .map(|c| {
            (
                c.claim.manifest_label.as_str(),
                c.binding.name.as_str(),
                c.glob,
                c.binding.files[c.glob].as_str(),
            )
        })
        .collect();
    parts.sort_unstable();
    let list: Vec<String> = parts
        .iter()
        .map(|(manifest, binding, glob, pattern)| {
            format!("{manifest} ({binding}, files[{glob}] = {pattern:?})")
        })
        .collect();
    format!(
        "{} manifests claim this file: {}",
        claimants.len(),
        list.join(", ")
    )
}

/// Rule 3: two or more live manifests claim `key` — denied, never a
/// nearest-wins shadow. An ERROR finding (NML2087 — coded like
/// every other finding, so `nml explain` can serve it), locationless,
/// attributed to the key: the file validates under NO binding and nothing runs against
/// it, so every verb counts it and exits 1 — a parse-only pass with a
/// green exit would be the same fail-open the truncation fix closed
/// (E28). Names every claimant; the operator narrows one.
pub(crate) fn ambiguous_claim(key: &SourceKey, claimants: &[Claimant<'_>]) -> Diagnostic {
    Diagnostic::error(format!(
        "{} — an ambiguously-claimed file is denied: it validates under no binding and \
         nothing runs against it; remove or narrow one claim (a `schemaPackages` pin in the \
         nearest live project config chooses between package names, never between two \
         manifests of one name)",
        ambiguous_claim_summary(claimants)
    ))
    .with_code(codes::AMBIGUOUS_CLAIM)
    .with_source(key.as_str().to_string())
}

/// NML2090 on `key`: `what` it is, and the one remedy — the gate's
/// sentence for content the walk left out of its enumeration by policy
/// under a directory a front end was asked to certify.
fn unjudged(key: &SourceKey, what: &str, remedy: &str) -> Diagnostic {
    Diagnostic::error(format!(
        "the walk skipped `{key}`: {what} — content a runtime could read that no verb judged; \
         {remedy}"
    ))
    .with_code(codes::UNJUDGED_CONTENT)
    .with_source(key.as_str().to_string())
}

/// The row for a symlink the walk left: a `.nml`-named one is an error
/// — the resolver's NML2083 under a closed universe (the same sentence
/// the link gets when named), NML2090 under an open one — and any other
/// a warning naming what the walk cannot know.
fn symlink_row(key: &SourceKey, closed: bool) -> Diagnostic {
    let name = key.file_name();
    if !is_nml_name(name) {
        return Diagnostic::warning(format!(
            "the walk skipped `{key}`: a symlink it did not enter — content beneath it, if any, \
             was not judged; replace the link with the content itself"
        ))
        .with_code(codes::UNJUDGED_CONTENT)
        .with_source(key.as_str().to_string());
    }
    let rejected = closed.then(|| {
        path_finding_typed(
            &PathError::SymlinkComponent {
                component: name.to_string(),
                key: key.clone(),
            },
            None,
        )
    });
    match rejected.flatten() {
        Some(row) => row,
        None => unjudged(
            key,
            "a symlink — followed only when named on the command line, never by a directory walk",
            "name it, or replace the link with the content itself",
        ),
    }
}

/// The gate's row for one entry the walk skipped AT its listing
/// ([`Skipped`]): a symlink (the resolver's NML2083 under a closed
/// universe, NML2090 under an open one), a `.nml` FIFO or a `.nml`
/// dot-file are rows; a dot-directory is not a row of its own — the
/// caller audits it ([`super::audit_hidden`]) and renders what it holds
/// through [`skipped_under`]; a policy directory (`node_modules`,
/// `target`) is a row on the closing row only, never a finding. Total
/// over [`Skip`]: a new reason cannot ship without a sentence.
pub fn skipped(entry: &Skipped, closed: bool) -> Option<Diagnostic> {
    let key = &entry.key;
    match &entry.why {
        Skip::Symlink => Some(symlink_row(key, closed)),
        Skip::Fifo => Some(unjudged(
            key,
            "a FIFO, socket or device",
            "replace it with a regular file",
        )),
        Skip::DotFile => Some(unjudged(
            key,
            "a dot-file, never checked, fixed or indexed unasked",
            "name it on the command line, or rename it",
        )),
        Skip::ComponentBound => Some(unjudged(
            key,
            &format!(
                "a directory at the {MAX_COMPONENTS}-component bound the walk never enters (nothing \
                 beneath it is keyable)"
            ),
            "flatten the tree, or move its content where the walk lists it",
        )),
        Skip::DotDirectory | Skip::PolicyDirectory => None,
        Skip::UnkeyableName { kind, name } => Some(unkeyable_row(key, name, *kind)),
    }
}

/// NML2090 for an entry whose name no key can carry
/// ([`Skip::UnkeyableName`]): attributed to the directory holding it
/// (`.` for the root), naming the entry lossily and what it is — a
/// directory the walk never entered (content beneath it, if any,
/// unjudged), a symlink, a `.nml`-named file or special entry — an
/// error, like every other entry a gate cannot judge; the remedy is a
/// plain name.
fn unkeyable_row(dir: &SourceKey, entry: &str, kind: EntryKind) -> Diagnostic {
    let what = match kind {
        EntryKind::Dir => "a directory the walk never entered",
        EntryKind::Symlink => "a symlink it never entered nor read",
        EntryKind::File => "a `.nml` file no verb judged",
        EntryKind::Other => "a `.nml`-named FIFO, socket or device",
    };
    Diagnostic::error(format!(
        "the walk skipped an entry under `{}` whose name no key can carry (`{entry}`: not \
         UTF-8, or bearing a path separator): {what} — content a runtime could read that no \
         verb judged; rename it with a plain name",
        dir.dir_label()
    ))
    .with_code(codes::UNJUDGED_CONTENT)
    .with_source(dir.dir_label().to_string())
}

/// The gate's ONE row for the skipped dot-directory `hidden` and what
/// its audit found beneath it ([`super::HiddenAudit`]): the directory,
/// the exact count of `.nml` entries beneath it (files, links and FIFOs
/// alike — content a runtime could read) and up to
/// [`MAX_AUDIT_EXAMPLES`](super::discover::MAX_AUDIT_EXAMPLES) of their
/// keys. One row per hidden directory,
/// never per file: a tenant's committed dot-directory of 300,000 `.nml`
/// files used to mint 300,000 error rows (~1 KB each, held and
/// deduplicated — 259 MB and 90 s at 300k, ~1 GB at the audit bound);
/// the gate needs the count and where. `None` when the audit found
/// nothing (an empty or `.nml`-free dot-directory is no finding).
pub fn skipped_under(hidden: &SourceKey, audit: &HiddenAudit) -> Option<Diagnostic> {
    if audit.nml == 0 {
        return None;
    }
    let examples: Vec<String> = audit.examples.iter().map(|k| format!("`{k}`")).collect();
    let more = audit.nml.saturating_sub(audit.examples.len());
    // Every counted name may be one no key can carry (never an
    // example): the count is then the whole sentence.
    let named = if examples.is_empty() {
        format!("{more} of them named by no key")
    } else if more > 0 {
        format!("{}, and {more} more", examples.join(", "))
    } else {
        examples.join(", ")
    };
    Some(
        Diagnostic::error(format!(
            "the walk skipped `{hidden}`: a dot-directory it never enters, holding {}{} `.nml` \
             file(s) no verb judged ({named}) — content a runtime could read; move it where the \
             walk lists it, or name the files on the command line",
            // An audit that could not finish counted what it could list:
            // the count is a lower bound and says so.
            if audit.incomplete.is_some() {
                "at least "
            } else {
                ""
            },
            audit.nml
        ))
        .with_code(codes::UNJUDGED_CONTENT)
        .with_source(hidden.as_str().to_string()),
    )
}

/// The gate's row for a hidden directory its audit could not finish at
/// `at` — the run's [`MAX_TOTAL_ENTRIES`] audit budget spent, or the
/// directory unlistable — an error: content hides exactly there.
pub fn audit_incomplete(at: &SourceKey) -> Diagnostic {
    unjudged(
        at,
        &format!(
            "a hidden directory the gate could not audit whole (the run's {MAX_TOTAL_ENTRIES}-entry \
             audit budget spent, unlistable, or holding a directory whose name no key can carry)"
        ),
        "remove it, move its content where the walk lists it, or name the content directories \
         rather than the tree that holds them",
    )
}
