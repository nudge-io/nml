//! Schema packages (RFC 0030): a versioned, self-describing bundle of
//! `.model.nml` sources plus a `<name>.package.nml` manifest declaring
//! everything needed to *construct* a validator — source files, file-pattern
//! bindings, schema-set composition, strictness, modifier keywords,
//! membership semantics, and directive vocabulary.
//!
//! One package, two consumers: a publisher (e.g. nudge) embeds its package
//! and builds its boot validators from it through this module; the LSP loads
//! the same package (from the per-user store or a workspace manifest) and
//! builds the same validators. Editor and server cannot drift, because both
//! execute one definition of "how is this config validated".

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use nml_core::ast::{
    ArrayDecl, Body, BodyEntryKind, DeclarationKind, File, Identifier, ListItemKind,
};
use nml_core::layers::LayerGrant;
use nml_core::span::Span;
use nml_core::types::{SpannedValue, Value};

use crate::fs::{MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES};
use crate::loader::load_schema;
use crate::schema::{MembershipSemantics, SchemaValidator};
use nml_core::diagnostic::{Code, Diagnostic, codes};
use nml_core::query::{BlockQuery, StringList};

/// The package-format version this build of nml understands. A manifest
/// declaring a newer `formatVersion` must be rejected *before*
/// meta-validation (`PackageError::UnsupportedFormatVersion`) so consumers
/// can degrade gracefully (RFC 0030's `formatVersion` contract).
pub const SUPPORTED_FORMAT_VERSION: u64 = 1;

/// The meta-schema validating `<name>.package.nml` manifests. Shipped as the
/// builtin package (see [`builtin_meta_package`]).
const PACKAGE_META_SCHEMA: &str = include_str!("../assets/package.model.nml");

/// One `[]schema` entry: logical name → source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    pub name: String,
    pub file: String,
}

/// One `[]validator` binding: the files it claims and the composed schema
/// set that validates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatorBinding {
    pub name: String,
    /// Root-relative glob patterns; first matching binding in list order wins.
    pub files: Vec<String>,
    /// Logical schema names, resolved against the `[]schema` declaration.
    pub schemas: Vec<String>,
    pub strict: bool,
    /// The binding's composition grant (RFC 0019): a binding without one
    /// denies composition (`GrantLookup::NoGrant`). Extracted from the
    /// manifest's `layers:` block (the loader's `extract_layers`; the
    /// block's shape is the meta-schema's, its own rules NML2081).
    pub layers: Option<LayerGrant>,
    /// The binding's name span in the manifest text — diagnostics about a
    /// binding (shadow warnings) point at the binding, not at (0,0).
    pub span: Span,
    /// The span of each `files` glob, parallel to `files` — a diagnostic
    /// about ONE glob (NML2092) points at that glob.
    pub file_spans: Vec<Span>,
}

/// Directive argument kinds (RFC 0032) — a closed set, mirrored by the
/// meta-schema's `directiveArg` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectiveArg {
    None,
    Ident,
    String,
    Number,
}

impl DirectiveArg {
    /// Human-facing name of the argument kind — one owner for the wording
    /// editor surfaces share (completion detail, hover, arity diagnostics).
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "no argument",
            Self::Ident => "ident",
            Self::String => "string",
            Self::Number => "number",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "ident" => Self::Ident,
            "string" => Self::String,
            "number" => Self::Number,
            _ => return None,
        })
    }
}

/// One `[]directive` vocabulary entry (RFC 0032 consumer registration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveDecl {
    pub name: String,
    pub arg: DirectiveArg,
    pub doc: String,
}

/// The typed manifest of a schema package.
#[derive(Debug, Clone)]
pub struct PackageManifest {
    pub name: String,
    /// Human label; never an identity (the content hash is).
    pub version: String,
    pub format_version: u64,
    /// Root-marker filenames anchoring binding globs to a project root.
    pub root_markers: Vec<String>,
    /// The manifest's BUDGET UNITS, declared (RFC 0019 A16/E38, item 4's
    /// spelling): anchor-relative DIRECTORY patterns — `*` within a
    /// segment, no `**`, every segment a directory — each directory at
    /// exactly that depth matching one is a unit root, in place of the
    /// inference from the binding globs' last wildcard run. Empty = infer.
    /// Never narrower than inference: the loader (`validate_budget_units`) refuses a
    /// declaration that would leave content a wildcard glob reaches in
    /// the root unit.
    pub budget_units: Vec<String>,
    pub modifiers: Vec<String>,
    pub membership: MembershipSemantics,
    pub schemas: Vec<SchemaEntry>,
    pub validators: Vec<ValidatorBinding>,
    pub directives: Vec<DirectiveDecl>,
}

/// The work ONE manifest's shadow analysis may spend, summed over every
/// glob pair it compares (`glob::subsumes_budgeted`'s currency: state
/// pairs explored times automaton size).
///
/// The per-comparison bound bounds no analysis that makes many, and this
/// one makes quadratically many: every binding's globs against every
/// EARLIER binding's. The manifest's own 256 KiB bound admits thousands
/// of declarations, so the pair count is the square of a number a
/// declaration list controls — measured, before this bound, at 108 s for
/// 447 declarations in a 64 KiB manifest, and 127 s for one
/// `textDocument/diagnostic` on it in the language server, which runs
/// this per pull on a manifest a repository wrote.
///
/// Past it every remaining pair reads as incomparable, so the analysis
/// reports FEWER shadow warnings — an authoring advisory withheld, never
/// a wrong verdict: nothing a shadow warning says gates anything the
/// walk, the binding or the validation decide.
///
/// The value is calibrated at both ends, by measurement on this machine.
/// A hostile manifest AT the 256 KiB bound (1,773 bindings whose globs
/// explode the subset construction) decides in 0.9 s, and one of three
/// MAXIMAL globs — 64 segments of 1 KiB, which alone cost 8 s a pair —
/// in 0.2 s; both were minutes before. A manifest of 60 bindings, an
/// order of magnitude past any real one, spends under a quarter of the
/// budget, so its advisories are untouched
/// (`a_realistic_manifest_never_reaches_the_shadow_budget` measures the
/// spend itself). A manifest of several hundred bindings loses shadow
/// advisories, which is the trade this bound makes on purpose: the
/// alternative is a language server a repository can stop.
///
/// The unit is not proportional to time across pattern shapes — a dense
/// subset over short patterns costs more per state pair than a sparse
/// one over long patterns — so the calibration is by measurement at both
/// extremes, not by arithmetic on the number.
///
/// LIMIT: reach=content guards=work surface=kernel shown="20000000" — subsumption work one manifest's shadow analysis may spend across every glob pair it compares
pub(crate) const MAX_SHADOW_WORK: usize = 20_000_000;

impl PackageManifest {
    /// RFC 0030 meta-validation: a binding fully shadowed by an earlier one
    /// can never win under first-match-wins — every one of its globs is
    /// subsumed by some earlier binding's glob (exact language inclusion,
    /// `glob::subsumes`). Conservative by construction: zero false positives.
    /// Spans are byte offsets into the manifest text — exactly the text an
    /// editor maps against when the resolved file IS the manifest, so the
    /// warning lands on the shadowed binding itself. Publishers may escalate
    /// severity (nudge's boot gate refuses to ship dead bindings); editors
    /// surface it as the warning it is. Same table, different stakes.
    ///
    /// The whole analysis is bounded by ONE budget (`MAX_SHADOW_WORK`),
    /// not by its per-comparison one: the comparisons are quadratic in a
    /// count the manifest declares.
    pub fn shadow_warnings(&self) -> Vec<Diagnostic> {
        let mut warnings = Vec::new();
        let mut budget = MAX_SHADOW_WORK;
        for (i, later) in self.validators.iter().enumerate() {
            let shadowed_by = self.validators[..i].iter().find(|earlier| {
                later.files.iter().all(|lg| {
                    earlier
                        .files
                        .iter()
                        .any(|eg| crate::glob::subsumes_budgeted(eg, lg, &mut budget))
                })
            });
            if let Some(earlier) = shadowed_by {
                warnings.push(
                    Diagnostic::warning(format!(
                        "validator '{}' is fully shadowed by earlier validator '{}' (first match wins) — it can never bind a file",
                        later.name, earlier.name
                    ))
            .with_code(nml_core::diagnostic::codes::SHADOWED_VALIDATOR)
                    .with_span(later.span),
                );
            }
        }
        warnings
    }
}

/// A loaded schema package: the manifest plus its schema sources, keyed by
/// logical name in declaration order (the hash covers them in this order).
#[derive(Debug, Clone)]
pub struct SchemaPackage {
    pub manifest: PackageManifest,
    /// `(logical name, source text)` in `[]schema` declaration order.
    ///
    /// The text is SHARED: one discovery loads a declared source
    /// once per `(directory, file)` and hands the same `Arc` to every
    /// live manifest that declares it. Without it, N manifests declaring
    /// one 4 MiB source retain N copies — measured 1,927 MB at N = 500,
    /// and the entry bound admits ~32k such manifests.
    pub sources: Vec<(String, Arc<str>)>,
    /// The manifest's own source text (hashed; also useful for re-display).
    pub manifest_text: String,
}

/// Why a package failed to load — RFC 0030's "package-load failure is a
/// named degraded state": every variant identifies what a diagnostic must
/// name (package, file, error) so consumers fall through to unbound
/// validation with one precise message instead of going dark.
#[derive(Debug, Clone)]
pub enum PackageError {
    /// The manifest declares a `formatVersion` newer than this build
    /// understands. Checked before meta-validation.
    UnsupportedFormatVersion { required: u64, supported: u64 },
    /// The manifest failed to parse, to meta-validate, or to satisfy a
    /// rule of the loader's own (a bad name charset, `[]validator.schemas`
    /// naming no declared schema, duplicate logical names, a missing
    /// required entry field, a malformed glob, a `budgetUnits` declaration
    /// narrower than inference, a `layers:` grant breaking its rules);
    /// every finding is a located diagnostic under its own code — the
    /// parser's, the meta-schema's, or the loader's (NML2081, NML2082,
    /// NML2093–NML2104) — and `at` locates the first in the manifest
    /// text.
    Manifest {
        errors: Vec<Diagnostic>,
        at: Option<ManifestLocation>,
    },
    /// A declared source file is missing or unreadable.
    MissingSource { file: String, detail: String },
    /// A declared source failed schema-loading (parse error, duplicate
    /// definitions, cycles). Diagnostics carry per-source attribution.
    Sources { errors: Vec<Diagnostic> },
}

/// Where a manifest's first finding sits: the 1-based line and column in
/// the manifest text, and the manifest's file once a loader that knows
/// it has named it ([`PackageError::in_file`], `from_dir`) — so a
/// caller with no row to place reads `manifest failed validation at
/// demo.package.nml:6:7: …`, the place to open next (it used to end in
/// a count, `(1 error(s))`, and name no location at all). A workspace
/// row states the location ONCE, as its own: the finding's span in the
/// manifest's text (`workspace::Discovery::manifest_location`), and its
/// sentence names no line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestLocation {
    pub file: Option<String>,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for ManifestLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(f, "{file}:")?;
        }
        write!(f, "{}:{}", self.line, self.column)
    }
}

impl ManifestLocation {
    /// The location of the first located finding among `errors`, in
    /// `text` (the manifest); `None` when none carries a span.
    fn first(errors: &[Diagnostic], text: &str) -> Option<Self> {
        let loc = locate_in(text, errors.iter().find_map(|d| d.span)?)?;
        Some(Self {
            file: None,
            line: loc.line,
            column: loc.column,
        })
    }
}

/// Where `span` sits in a manifest's `text` (1-based line and column) —
/// the ONE derivation behind every location a manifest finding shows:
/// the library sentence's, the CLI's `key:line:col:` and `--json`
/// `line`/`col` (through `Discovery::manifest_location`). `None` past the
/// text (a stale offset is never indexed).
pub(crate) fn locate_in(text: &str, span: Span) -> Option<nml_core::span::Location> {
    (span.start <= text.len()).then(|| nml_core::span::SourceMap::new(text).location(span.start))
}

impl PackageError {
    /// Name the manifest file a [`PackageError::Manifest`] finding sits
    /// in — a loader that knows the file and has no row to place
    /// (`from_dir`, by its path) says so; every other variant is
    /// returned unchanged. A workspace row carries the finding's span
    /// instead ([`crate::workspace::Discovery::manifest_location`]).
    pub fn in_file(self, file: &str) -> Self {
        match self {
            Self::Manifest {
                errors,
                at: Some(at),
            } => Self::Manifest {
                errors,
                at: Some(ManifestLocation {
                    file: Some(file.to_string()),
                    ..at
                }),
            },
            other => other,
        }
    }
}

/// `PackageError` participates in standard error handling (`?` into
/// `Box<dyn Error>`): every variant is self-contained (diagnostics and
/// strings), so there is no `source()` chain.
impl std::error::Error for PackageError {}

impl PackageError {
    /// The formatVersion gate as a coded finding (NML2102). The gate is
    /// stated as values — `required`, `supported`, which a consumer
    /// matches to say what to update — and a row that wraps the refusal
    /// ([`Diagnostic::caused_by`]) carries this finding as its cause, so
    /// the code rides the wire like every other loader rule's. `None`
    /// for every other variant: a `Manifest` and a `Sources` carry their
    /// findings, a `MissingSource` is a sentence about a file.
    pub fn gate_finding(&self) -> Option<Diagnostic> {
        match self {
            Self::UnsupportedFormatVersion { .. } => Some(
                Diagnostic::error(self.to_string()).with_code(codes::UNSUPPORTED_FORMAT_VERSION),
            ),
            _ => None,
        }
    }
}

/// How many findings a load error has, said where it belongs: qualifying
/// the FAILURE, ahead of the colon that introduces the first finding —
/// `manifest failed validation (finding 1 of 2): unknown property 'versio'`.
/// Nothing for a single finding; a one-finding manifest has no count to
/// read, and the old `(1 error(s))` told its reader nothing to act on.
///
/// It is a HEAD and not a tail because the renderer appends a
/// did-you-mean to the END of a message (hint prose never enters a
/// message — `diagnostic`'s module rule), so a count sitting there read
/// as the thing the hint repaired: `… 'versio' (and 1 more) (did you mean
/// "version"?)`. Ahead of the colon, the hint closes the finding it
/// repairs. THE builder for all four sentences — the two library ones and
/// the two workspace rows (NML2088, NML2091), which each used to carry
/// their own copy.
pub(crate) fn finding_of(count: usize) -> String {
    match count {
        0 | 1 => String::new(),
        n => format!(" (finding 1 of {n})"),
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormatVersion {
                required,
                supported,
            } => write!(
                f,
                "package requires formatVersion {required}; this nml supports {supported}"
            ),
            Self::Manifest { errors, at } => match errors.first() {
                Some(first) => {
                    write!(f, "manifest failed validation")?;
                    if let Some(at) = at {
                        write!(f, " at {at}")?;
                    }
                    write!(f, "{}: {}", finding_of(errors.len()), first.message)
                }
                None => write!(f, "manifest failed validation"),
            },
            Self::MissingSource { file, detail } => {
                write!(f, "declared source '{file}' is unavailable: {detail}")
            }
            Self::Sources { errors } => match errors.first() {
                Some(first) => {
                    write!(
                        f,
                        "schema sources failed to load{}: {}{}",
                        finding_of(errors.len()),
                        first
                            .source
                            .as_deref()
                            .map(|s| format!("{s}: "))
                            .unwrap_or_default(),
                        first.message
                    )
                }
                None => write!(f, "schema sources failed to load"),
            },
        }
    }
}

impl SchemaPackage {
    /// Load a package from a manifest text plus its source files, resolved by
    /// the caller (embedded `include_str!` bundles, the store, a workspace).
    /// `resolve` maps a declared file name to its contents.
    /// `resolve` may hand back an owned `String` (the common case: a
    /// bundle, the store, a test) or an `Arc<str>` a caller is sharing
    /// across packages (workspace discovery's per-`(directory, file)`
    /// cache) — the package retains whichever without copying it again.
    pub fn from_parts<T: Into<Arc<str>>>(
        manifest_text: &str,
        mut resolve: impl FnMut(&str) -> Result<T, String>,
    ) -> Result<Self, PackageError> {
        let manifest = parse_manifest(manifest_text)?;
        let mut sources = Vec::with_capacity(manifest.schemas.len());
        for entry in &manifest.schemas {
            let text = resolve(&entry.file).map_err(|detail| PackageError::MissingSource {
                file: entry.file.clone(),
                detail,
            })?;
            sources.push((entry.name.clone(), text.into()));
        }
        Ok(Self {
            manifest,
            sources,
            manifest_text: manifest_text.to_string(),
        })
    }

    /// Load a package from a directory holding `<name>.package.nml` and its
    /// declared sources.
    ///
    /// Every byte comes through the crate's ONE reader
    /// ([`crate::fs::read_beneath`]), beneath `dir`, under the same two
    /// bounds the workspace walk reads a manifest and a declared source
    /// under ([`MAX_MANIFEST_BYTES`], [`MAX_SOURCE_BYTES`]): a store slot
    /// is read exactly as a live package is. The by-path
    /// `read_to_string` this replaces was UNBOUNDED and followed a link
    /// or blocked forever on a FIFO named `x.model.nml` — in the editor,
    /// on the store-load path, with no way out.
    pub fn from_dir(dir: &Path) -> Result<Self, PackageError> {
        let manifest_name = find_manifest(dir)?;
        let manifest_path = dir.join(&manifest_name);
        let missing = |file: &Path| {
            let file = file.display().to_string();
            move |detail: String| PackageError::MissingSource { file, detail }
        };
        let manifest_text = crate::fs::read_beneath(
            dir,
            &[manifest_name.as_str()],
            MAX_MANIFEST_BYTES,
            "a package manifest",
        )
        .map_err(|e| missing(&manifest_path)(e.to_string()))?;
        Self::from_parts(&manifest_text, |file| {
            check_plain_file_name(file)?;
            crate::fs::read_beneath(dir, &[file], MAX_SOURCE_BYTES, "a declared schema source")
                .map_err(|e| e.to_string())
        })
        .map_err(|e| e.in_file(&manifest_path.display().to_string()))
    }

    /// The package's content hash (RFC 0030 framing, bit-exact across
    /// publisher and consumer): blake3 over length-prefixed
    /// `(name, len, LF-normalized bytes)` frames — the manifest first (named
    /// by its manifest filename convention), then each `[]schema` source in
    /// declaration order. No other canonicalization: the hash identifies
    /// bytes, not style.
    pub fn content_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        let mut frame = |name: &str, text: &str| {
            let normalized = text.replace("\r\n", "\n");
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
            hasher.update(&(normalized.len() as u64).to_le_bytes());
            hasher.update(normalized.as_bytes());
        };
        frame(
            &format!("{}.package.nml", self.manifest.name),
            &self.manifest_text,
        );
        for (name, text) in &self.sources {
            frame(name, text);
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }

    /// The composed, inheritance-resolved schema set for one binding: the
    /// models/enums/oneofs named by `binding.schemas`, merged and with
    /// `extends` resolved — exactly what [`Self::validator`] builds from.
    /// Exposed for consumers that need the schema *itself* rather than a
    /// validator (e.g. a config differ / reload classifier), so they share the
    /// manifest as the single source of truth and cannot drift from validation.
    /// Binding is exclusive by construction — only the package's own sources
    /// participate.
    pub fn composed_schema(
        &self,
        binding: &ValidatorBinding,
    ) -> Result<nml_core::schema::ExtractedSchema, PackageError> {
        let selected: Vec<(&str, &str)> = self
            .sources
            .iter()
            .filter(|(name, _)| binding.schemas.iter().any(|s| s == name))
            .map(|(name, text)| (name.as_str(), &**text))
            .collect();
        let (schema, diags) = load_schema(&selected);
        let errors: Vec<Diagnostic> = diags
            .into_iter()
            .filter(|d| matches!(d.severity, nml_core::diagnostic::Severity::Error))
            .collect();
        if !errors.is_empty() {
            return Err(PackageError::Sources { errors });
        }
        Ok(schema)
    }

    /// Build the validator for one binding: the [`Self::composed_schema`] set
    /// with the package's profile applied (strictness, modifiers, membership).
    pub fn validator(&self, binding: &ValidatorBinding) -> Result<SchemaValidator, PackageError> {
        let schema = self.composed_schema(binding)?;
        // Package bindings are a closed vocabulary (RFC 0012): the operator's
        // composed schemas are the entire authority for bound files.
        let mut validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
            .closed_vocabulary()
            .with_modifiers(self.manifest.modifiers.clone())
            .with_membership_semantics(self.manifest.membership.clone());
        if binding.strict {
            validator = validator.strict();
        }
        Ok(validator)
    }

    /// The first binding whose glob set matches `path` (root-relative,
    /// `/`-normalized) — first match in declaration order wins.
    pub fn binding_for(&self, path: &str) -> Option<&ValidatorBinding> {
        self.binding_match(path).map(|(binding, _)| binding)
    }

    /// [`Self::binding_for`] plus the index of the matching `files` glob —
    /// the referent `nml binding` prints (`files[1] = "…"`) and the
    /// D-0d-1 conflict names; grant rules are unnamed strings, so the
    /// index is the only stable handle.
    pub fn binding_match(&self, path: &str) -> Option<(&ValidatorBinding, usize)> {
        self.manifest.validators.iter().find_map(|b| {
            b.files
                .iter()
                .position(|g| crate::glob::glob_match(g, path))
                .map(|i| (b, i))
        })
    }
}

/// The builtin meta package: nml's own `package.model.nml`, delivered through
/// the package mechanism itself. Its binding claims `*.package.nml` (which a
/// bare `package.nml` deliberately does not match), strict and exclusive,
/// through the same authority ladder as every other package.
pub fn builtin_meta_package() -> SchemaPackage {
    const BUILTIN_MANIFEST: &str = "\
package nml:
    version = \"1\"
    formatVersion = 1

[]schema schemas:
    - package:
        file = \"package.model.nml\"

[]validator validators:
    - package:
        files:
            - \"**/*.package.nml\"
        schemas:
            - package
        strict = true
";
    SchemaPackage::from_parts(BUILTIN_MANIFEST, |file| {
        debug_assert_eq!(file, "package.model.nml");
        Ok(PACKAGE_META_SCHEMA.to_string())
    })
    .expect("builtin meta package must load")
}

/// Package names become store path components and written pin entries: a
/// strict lowercase identifier (`[a-z][a-z0-9-]*`) is what makes those
/// interpolations injection-proof (RFC 0030 Security). Enforced on manifests
/// at extraction AND on every externally-supplied name (pins, store lookups)
/// before it touches a path.
pub fn valid_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

/// The one manifest NAME in `dir` (a plain component, so the caller can
/// read it beneath `dir` through the chain). Listed through the kernel's
/// ONE listing rule ([`crate::fs::listing`]): sorted, and an entry whose
/// kind cannot be read refuses the whole listing rather than vanishing —
/// a dropped entry could hide the second manifest this refuses.
fn find_manifest(dir: &Path) -> Result<String, PackageError> {
    let entries =
        crate::fs::listing(std::fs::read_dir(dir)).map_err(|e| PackageError::MissingSource {
            file: dir.display().to_string(),
            detail: e.to_string(),
        })?;
    let mut manifests: Vec<String> = entries
        .into_iter()
        .filter_map(|(name, _)| name.to_str().map(str::to_string))
        .filter(|n| crate::file_names::is_manifest_name(n))
        .collect();
    manifests.sort();
    match manifests.len() {
        0 => Err(PackageError::MissingSource {
            file: dir.display().to_string(),
            detail: "no <name>.package.nml manifest found".to_string(),
        }),
        1 => Ok(manifests.remove(0)),
        _ => Err(PackageError::Manifest {
            errors: vec![*rule(
                codes::MULTIPLE_MANIFESTS,
                format!(
                    "package directory holds {} manifests; exactly one <name>.package.nml is allowed",
                    manifests.len()
                ),
                None,
            )],
            at: None,
        }),
    }
}

/// Declared source file names are plain names, never paths: a manifest can
/// never reach outside its own directory. Shared by every resolution path
/// (store slots, workspace manifests, embedded bundles).
pub fn check_plain_file_name(file: &str) -> Result<(), String> {
    if file.contains('/') || file.contains('\\') || file.contains("..") || file.is_empty() {
        return Err("declared file names must be plain file names".to_string());
    }
    Ok(())
}

/// Parse and meta-validate a manifest into its typed form.
///
/// Order matters (RFC 0030): the `formatVersion` gate runs *before*
/// meta-validation so a newer publisher degrades an older consumer with one
/// precise error, never a wall of unknown-key noise.
pub fn parse_manifest(text: &str) -> Result<PackageManifest, PackageError> {
    let file = match nml_core::cst::parse_to_ast(text) {
        Ok(file) => file,
        Err(e) => {
            // A future formatVersion may change *syntax*; the degradation
            // contract must still produce the one precise gate error, not a
            // parse-error wall. Cheap text scan, only on the failure path.
            if let Some(fv) = scan_format_version_text(text) {
                if fv > SUPPORTED_FORMAT_VERSION {
                    return Err(PackageError::UnsupportedFormatVersion {
                        required: fv,
                        supported: SUPPORTED_FORMAT_VERSION,
                    });
                }
            }
            let errors = vec![e.to_diagnostic()];
            let at = ManifestLocation::first(&errors, text);
            return Err(PackageError::Manifest { errors, at });
        }
    };

    // formatVersion gate — a cheap pre-scan of the package block.
    if let Some(fv) = scan_format_version(&file) {
        if fv > SUPPORTED_FORMAT_VERSION {
            return Err(PackageError::UnsupportedFormatVersion {
                required: fv,
                supported: SUPPORTED_FORMAT_VERSION,
            });
        }
    }

    // Meta-validation: the manifest is an instance of the meta-schema.
    let (meta, meta_diags) = load_schema(&[("package.model.nml", PACKAGE_META_SCHEMA)]);
    debug_assert!(
        meta_diags.is_empty(),
        "embedded meta-schema must load clean: {meta_diags:?}"
    );
    let validator = SchemaValidator::new(meta.models, meta.enums, meta.oneofs)
        .closed_vocabulary()
        .strict();
    let errors: Vec<Diagnostic> = validator
        .validate(&file)
        .into_iter()
        .filter(|d| matches!(d.severity, nml_core::diagnostic::Severity::Error))
        .collect();
    if !errors.is_empty() {
        let at = ManifestLocation::first(&errors, text);
        return Err(PackageError::Manifest { errors, at });
    }

    extract_manifest(&file).map_err(|finding| {
        let finding = *finding;
        let at = ManifestLocation::first(std::slice::from_ref(&finding), text);
        PackageError::Manifest {
            errors: vec![finding],
            at,
        }
    })
}

/// A loader rule's finding under its own code, located at `span` when
/// the rule has one (the offending block, item or name), so the
/// manifest's NML2088 row names its line and column like a
/// meta-validation finding does and carries the code as its cause
/// ([`Diagnostic::caused_by`]). The code is a parameter, never an
/// option: a rule the loader states as a finding rides the wire as a
/// fact a consumer acts on, not as a sentence alone.
fn rule(code: Code, message: String, span: Option<Span>) -> Box<Diagnostic> {
    let finding = Diagnostic::error(message).with_code(code);
    Box::new(match span {
        Some(span) => finding.with_span(span),
        None => finding,
    })
}

/// A manifest declares `package` and each of `[]schema`, `[]validator`
/// and `[]directive` once: a second declaration used to REPLACE the
/// first silently (the last array by keyword, the last `package`
/// block). Refused at the later keyword, the first a related note,
/// through the manifest's NML2088 row like every loader rule (NML2094,
/// the row's cause). This is the manifest's SHAPE — one slot per
/// keyword, whatever the names; a
/// second declaration under the SAME name never reaches here: the
/// parse refuses it (NML1000, the file-scope rule).
fn declared_once<'a, T>(
    slot: &mut Option<&'a T>,
    decl: &'a T,
    spelled: &str,
    keyword: &Identifier,
    first_keyword: impl FnOnce(&T) -> Span,
) -> Result<(), Box<Diagnostic>> {
    if let Some(first) = *slot {
        return Err(Box::new(
            Diagnostic::error(format!(
                "`{spelled}` is declared twice — a manifest declares `package` and each of \
                 `[]schema`, `[]validator` and `[]directive` once"
            ))
            .with_code(codes::REPEATED_DECLARATION)
            .with_span(keyword.span)
            .with_related(
                first_keyword(first),
                format!("`{spelled}` first declared here"),
            ),
        ));
    }
    *slot = Some(decl);
    Ok(())
}

/// The entry-name rule (NML2093) over a manifest's named items: a
/// `[]schema` source, a `[]validator` binding or a `[]directive` entry is its name — the
/// sources a binding lists, the binding `nml binding` prints — never a
/// positional element, so a second item so named is refused at the
/// later item with the first as a related note, through the manifest's
/// NML2088 row like every loader rule. Records `name` as seen.
fn name_once<'a>(
    named: &mut HashMap<&'a str, Span>,
    kind: &str,
    name: &'a Identifier,
) -> Result<(), Box<Diagnostic>> {
    match named.entry(name.name.as_str()) {
        Entry::Occupied(first) => Err(Box::new(
            Diagnostic::error(format!(
                "duplicate {kind} '{}' — a manifest names each {kind} once",
                name.name
            ))
            .with_code(codes::DUPLICATE_ENTRY)
            .with_span(name.span)
            .with_related(*first.get(), format!("'{}' first declared here", name.name)),
        )),
        Entry::Vacant(slot) => {
            slot.insert(name.span);
            Ok(())
        }
    }
}

/// Text-level formatVersion scan for manifests that fail to parse.
fn scan_format_version_text(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("formatVersion")?.trim_start();
        let digits = rest.strip_prefix('=')?.trim();
        digits.parse().ok().or_else(|| {
            // Saturate like `number_as_u64` below: a formatVersion too
            // wide for u64 IS newer than anything supported, so the
            // degradation gate must fire — not the parse-error wall
            // this fallback exists to preempt.
            (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(u64::MAX)
        })
    })
}

fn scan_format_version(file: &File) -> Option<u64> {
    for decl in &file.declarations {
        if let DeclarationKind::Block(block) = &decl.kind {
            if block.keyword.name == "package" {
                for entry in &block.body.entries {
                    if let BodyEntryKind::Property(p) = &entry.kind {
                        if p.name.name == "formatVersion" {
                            return number_as_u64(&p.value);
                        }
                    }
                }
            }
        }
    }
    None
}

/// `None` means **not a number at all**; a number too large for `u64`
/// saturates rather than vanishing. Since RFC 0016 made integers exact to
/// 34 digits, `formatVersion = <huge>` parses instead of failing at
/// NML0014 — and a `None` here would read downstream as "missing
/// `formatVersion`" while silently skipping the degradation gate, which
/// is precisely the wall-of-noise outcome RFC 0030 exists to prevent. A
/// version beyond `u64` is definitionally newer than anything this build
/// supports, so saturating keeps the one-precise-error contract intact.
/// The same reasoning covers negative and fractional values: none is a
/// version this build knows, and reporting "newer than supported" beats
/// "missing" for a field that is plainly present.
fn number_as_u64(v: &SpannedValue) -> Option<u64> {
    match &v.value {
        Value::Number(n) => Some(n.to_u64().unwrap_or(u64::MAX)),
        _ => None,
    }
}

fn extract_manifest(file: &File) -> Result<PackageManifest, Box<Diagnostic>> {
    let mut package_block = None;
    let mut schemas_decl: Option<&ArrayDecl> = None;
    let mut validators_decl: Option<&ArrayDecl> = None;
    let mut directives_decl: Option<&ArrayDecl> = None;

    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) if block.keyword.name == "package" => {
                declared_once(&mut package_block, block, "package", &block.keyword, |b| {
                    b.keyword.span
                })?;
            }
            DeclarationKind::Array(arr) => {
                let slot = match arr.item_keyword.name.as_str() {
                    "schema" => &mut schemas_decl,
                    "validator" => &mut validators_decl,
                    "directive" => &mut directives_decl,
                    _ => continue,
                };
                let spelled = format!("[]{}", arr.item_keyword.name);
                declared_once(slot, arr, &spelled, &arr.item_keyword, |a| {
                    a.item_keyword.span
                })?;
            }
            _ => {}
        }
    }

    let block = package_block.ok_or_else(|| {
        rule(
            codes::MISSING_DECLARATION,
            "manifest has no `package <name>:` block".to_string(),
            None,
        )
    })?;
    let name = block.name.name.clone();
    if !valid_package_name(&name) {
        return Err(rule(
            codes::INVALID_PACKAGE_NAME,
            format!("package name '{name}' is not a lowercase identifier ([a-z][a-z0-9-]*)"),
            Some(block.name.span),
        ));
    }

    let mut version = None;
    let mut format_version = None;
    let mut membership = MembershipSemantics::default();
    for entry in &block.body.entries {
        match &entry.kind {
            BodyEntryKind::Property(p) => match p.name.name.as_str() {
                "version" => version = string_value(&p.value),
                "formatVersion" => format_version = number_as_u64(&p.value),
                _ => {}
            },
            BodyEntryKind::NestedBlock(nb) if nb.name.name == "membership" => {
                membership = extract_membership(&nb.body)?;
            }
            _ => {}
        }
    }
    // Every list-valued field reads through the language's one accessor
    // (both spellings the meta-schema accepts for `[]string`), so the
    // loader cannot disagree with the schema it just enforced.
    let lists = BlockQuery::Found(&block.body);
    let root_markers = strings(manifest_list(&lists, "rootMarkers")?);
    let budget_units = manifest_list(&lists, "budgetUnits")?;
    let budget_units_span = budget_units.as_ref().map(|list| list.key);
    let budget_units = strings(budget_units);
    let modifiers = strings(manifest_list(&lists, "modifiers")?);
    let version = version.ok_or_else(|| {
        rule(
            codes::MISSING_REQUIRED_FIELD,
            "package block is missing `version`".to_string(),
            Some(block.name.span),
        )
    })?;
    let format_version = format_version.ok_or_else(|| {
        rule(
            codes::MISSING_REQUIRED_FIELD,
            "package block is missing `formatVersion`".to_string(),
            Some(block.name.span),
        )
    })?;

    let schemas = extract_schemas(schemas_decl)?;
    if schemas.is_empty() {
        return Err(rule(
            codes::MISSING_DECLARATION,
            "manifest declares no `[]schema` sources".to_string(),
            Some(block.name.span),
        ));
    }
    let declared: HashSet<&str> = schemas.iter().map(|s| s.name.as_str()).collect();

    let validators = extract_validators(validators_decl, &declared)?;
    let directives = extract_directives(directives_decl)?;
    validate_budget_units(&budget_units, &validators).map_err(|message| {
        rule(
            codes::BUDGET_UNIT_RULE,
            message,
            budget_units_span.or(Some(block.name.span)),
        )
    })?;

    Ok(PackageManifest {
        name,
        version,
        format_version,
        root_markers,
        budget_units,
        modifiers,
        membership,
        schemas,
        validators,
        directives,
    })
}

/// The load-time rules of a `budgetUnits` declaration (RFC 0019 E38,
/// item 4). *Syntax:* a non-empty `/`-separated directory pattern, every
/// segment a plain name or a `*`-bearing name — never empty, `.`, `..`,
/// `\\`-bearing or `**` (a unit has ONE depth; the key vocabulary) — at
/// most `MAX_PATTERN_SEGMENTS`.
/// *Never narrower than inference* (the one direction that exposes the
/// operator: content between a shallower delegation boundary and a
/// deeper declared unit would be the root unit's, and one tenant's
/// entries there would deny everyone): for every binding glob whose
/// directory segments bear a wildcard, some declared unit must sit at
/// or ABOVE the glob's inferred boundary (the start of its last
/// wildcard run) and cover every directory the glob reaches at the
/// unit's depth — segment-wise, a `*` unit segment covers anything, a
/// literal unit segment only the same literal. A glob with `**` among
/// those directory segments reaches directories at every depth and no
/// finite declaration covers it: refused, naming the spelling to fix.
/// A declaration SHALLOWER than inference (one unit for all tenants) is
/// the operator's coarser blast radius and is allowed; a wildcard-free
/// glob delegates nothing and constrains nothing.
fn validate_budget_units(units: &[String], validators: &[ValidatorBinding]) -> Result<(), String> {
    for unit in units {
        let segments: Vec<&str> = unit.split('/').collect();
        if unit.is_empty() || segments.len() > crate::glob::MAX_PATTERN_SEGMENTS {
            return Err(format!(
                "budgetUnits entry {:?} is not a directory pattern (1 to {} `/`-separated segments)",
                glob_echo(unit),
                crate::glob::MAX_PATTERN_SEGMENTS
            ));
        }
        for segment in &segments {
            if !crate::glob::is_plain_name(segment) || segment.contains("**") {
                return Err(format!(
                    "budgetUnits entry {:?}: segment {:?} — a unit is a directory at ONE \
                     depth (plain names and `*`, never empty, `.`, `..`, `**` or `\\`-bearing)",
                    glob_echo(unit),
                    glob_echo(segment)
                ));
            }
            // As for a binding glob: one segment's LENGTH is bounded
            // too, and its bytes are counted, never echoed.
            if segment.len() > crate::glob::MAX_PATTERN_SEGMENT_BYTES {
                return Err(format!(
                    "budgetUnits entry {:?}: a segment of {} bytes exceeds the {}-byte bound on \
                     one segment",
                    glob_echo(unit),
                    segment.len(),
                    crate::glob::MAX_PATTERN_SEGMENT_BYTES
                ));
            }
        }
    }
    // One unit, one budget (RFC 0026 B-3): a declared unit nesting inside
    // another without pinning a wildcard of it to a literal would let
    // every directory the outer unit delegates mint units beneath itself.
    for outer in units {
        for inner in units {
            if crate::glob::unit_nests_inside(outer, inner) {
                return Err(format!(
                    "budgetUnits {units:?}: unit {inner:?} nests inside unit {outer:?} — every \
                     directory the outer unit delegates would mint units of its own beneath it, \
                     multiplying its share of the walk's budget up to the universe-wide backstop; \
                     declare one of them, or pin the inner unit to a named directory (a literal \
                     where the outer unit has a `*`)"
                ));
            }
        }
    }
    if units.is_empty() {
        return Ok(());
    }
    for binding in validators {
        for glob in &binding.files {
            let Some(boundary) = crate::glob::unit_prefix_len(glob) else {
                continue;
            };
            let segments: Vec<&str> = glob.split('/').collect();
            let directories = if segments.last() == Some(&"**") {
                segments.len()
            } else {
                segments.len().saturating_sub(1)
            };
            let dirs = &segments[..directories];
            // A `**` AT the boundary is the delegation point itself (a
            // `*` unit segment covers it); one BEFORE the boundary has no
            // fixed depth to declare.
            let unbounded_above = dirs[..boundary].contains(&"**");
            let covered = !unbounded_above
                && units.iter().any(|unit| {
                    let u: Vec<&str> = unit.split('/').collect();
                    u.len() <= boundary + 1
                        && u.iter()
                            .zip(dirs)
                            .all(|(us, gs)| *us == "*" || (*gs != "**" && us == gs))
                });
            if !covered {
                let inferred = dirs[..=boundary]
                    .iter()
                    .map(|s| if s.contains('*') { "*" } else { *s })
                    .collect::<Vec<_>>()
                    .join("/");
                return Err(if unbounded_above {
                    format!(
                        "budgetUnits {units:?} cannot cover files glob {glob:?} of validator '{}': \
                         it reaches directories at every depth — spell the layout with a fixed \
                         depth (`*`, not `**`) above its unit boundary",
                        binding.name
                    )
                } else {
                    format!(
                        "budgetUnits {units:?} leave content of files glob {glob:?} of validator \
                         '{}' in the root unit (its inferred unit is `{inferred}`): declare a \
                         unit at or above it, `{inferred}` say — a declaration never narrows \
                         isolation",
                        binding.name
                    )
                });
            }
        }
    }
    Ok(())
}

fn extract_schemas(decl: Option<&ArrayDecl>) -> Result<Vec<SchemaEntry>, Box<Diagnostic>> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut named: HashMap<&str, Span> = HashMap::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(rule(
                codes::UNNAMED_ENTRY,
                "`[]schema` entries must be named items (`- name:`)".to_string(),
                Some(item.span),
            ));
        };
        name_once(&mut named, "schema", name)?;
        // Loader-level required-field backstop (RFC 0030 spike finding): a
        // `file`-less entry must fail here with a precise message, not later
        // as an opaque read error.
        let declared = body_property(body, "file");
        let file = declared.and_then(string_value).ok_or_else(|| {
            rule(
                codes::MISSING_REQUIRED_FIELD,
                format!("`[]schema` entry '{}' is missing `file`", name.name),
                Some(name.span),
            )
        })?;
        // The one schema-source admission (`file_names::is_schema_source_name`)
        // is the LOADER's too: a declared source spelled outside it is a source
        // only this reader would have called one — the walk, the `--schema` scan
        // and the editor's gate all read the suffix, so the file would be judged
        // on the command line and ignored in the editor. Refused here, at the
        // value, so "one admission" is structural in every reader.
        if !crate::file_names::is_schema_source_name(&file) {
            return Err(rule(
                codes::INVALID_SCHEMA_SOURCE_NAME,
                format!(
                    "`[]schema` entry '{}' declares {file:?}, which is not spelled as a \
                     schema source ({})",
                    name.name,
                    crate::file_names::SCHEMA_SOURCE_SUFFIXES.join(", ")
                ),
                declared.map(|v| v.span),
            ));
        }
        out.push(SchemaEntry {
            name: name.name.clone(),
            file,
        });
    }
    Ok(out)
}

fn extract_validators(
    decl: Option<&ArrayDecl>,
    declared_schemas: &HashSet<&str>,
) -> Result<Vec<ValidatorBinding>, Box<Diagnostic>> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut named: HashMap<&str, Span> = HashMap::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(rule(
                codes::UNNAMED_ENTRY,
                "`[]validator` entries must be named items (`- name:`)".to_string(),
                Some(item.span),
            ));
        };
        name_once(&mut named, "validator", name)?;
        let lists = BlockQuery::Found(body);
        let (files, file_spans): (Vec<String>, Vec<Span>) = manifest_list(&lists, "files")?
            .map(|list| {
                list.items
                    .iter()
                    .map(|item| (item.text.to_string(), item.span))
                    .unzip()
            })
            .unwrap_or_default();
        let schemas = strings(manifest_list(&lists, "schemas")?);
        if files.is_empty() || schemas.is_empty() {
            return Err(rule(
                codes::EMPTY_BINDING,
                format!(
                    "`[]validator` entry '{}' needs non-empty `files` and `schemas`",
                    name.name
                ),
                Some(name.span),
            ));
        }
        for (glob, span) in files.iter().zip(&file_spans) {
            if let Err(why) = glob_rule(glob) {
                return Err(rule(
                    codes::INVALID_BINDING_GLOB,
                    format!(
                        "validator '{}' glob '{}': {why}",
                        name.name,
                        glob_echo(glob)
                    ),
                    Some(*span),
                ));
            }
        }
        for s in &schemas {
            if !declared_schemas.contains(s.as_str()) {
                return Err(rule(
                    codes::UNDECLARED_SCHEMA,
                    format!(
                        "validator '{}' names schema '{s}', which no `[]schema` entry declares",
                        name.name
                    ),
                    Some(name.span),
                ));
            }
        }
        out.push(ValidatorBinding {
            name: name.name.clone(),
            files,
            schemas,
            strict: body_property_bool(body, "strict").unwrap_or(false),
            layers: extract_layers(body, &name.name)?,
            span: name.span,
            file_spans,
        });
    }
    Ok(out)
}

/// Bytes of a glob a loader message carries verbatim before it is
/// elided. A `files`, `allowRefs`, `denyRefs` or `budgetUnits` entry is
/// author-supplied text bounded only by the manifest cap (256 KiB), and
/// a message that refuses one for being too long must not put it whole
/// on the terminal and the NDJSON wire — twice, as a `message` and a
/// `cause.message`.
///
/// LIMIT: reach=content guards=output surface=kernel shown="160 bytes" — bytes of a manifest glob a loader finding echoes before eliding its middle
pub const MAX_GLOB_ECHO_BYTES: usize = 160;

/// A glob as a MESSAGE spells it: itself when short, else its head and
/// its tail around its exact byte count. Cut on CHARACTER boundaries —
/// a glob is arbitrary UTF-8 and slicing one mid-character panics.
fn glob_echo(glob: &str) -> String {
    if glob.len() <= MAX_GLOB_ECHO_BYTES {
        return glob.to_string();
    }
    let mut head = MAX_GLOB_ECHO_BYTES * 2 / 3;
    while head > 0 && !glob.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = glob.len() - MAX_GLOB_ECHO_BYTES / 4;
    while tail < glob.len() && !glob.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}…({} bytes)…{}", &glob[..head], glob.len(), &glob[tail..])
}

/// The matcher's own rules for a manifest glob — `files`, `allowRefs` and
/// `denyRefs` alike (one glob vocabulary, RFC 0019): `**` is a
/// whole-segment wildcard only (embedded in a segment it would silently
/// degrade to `*`), every segment is one a key component can equal —
/// never empty, `.`, `..` or `\\`-bearing, the key vocabulary
/// ([`crate::glob::is_plain_name`]) — and the segment count
/// is capped where the matcher caps it, so a rejected pattern never
/// reaches it. A pattern the matcher can never satisfy matches nothing,
/// which is fail-closed for `files` and an allow rule (a binding or a
/// grant that silently claims nothing) but FAIL-OPEN for a deny rule: a
/// veto spelled `vendor\\secret\\**` or `vendor/./secret/**` loaded and
/// never fired.
fn glob_rule(glob: &str) -> Result<(), String> {
    if glob.split('/').any(|seg| seg.contains("**") && seg != "**") {
        return Err("`**` must be a whole segment".to_string());
    }
    if let Some(seg) = glob.split('/').find(|seg| !crate::glob::is_plain_name(seg)) {
        // Spelled through the same elision as the whole glob: this arm
        // runs BEFORE the byte bound, so a `\`-bearing segment of any
        // length reaches it, and its bytes rode the row twice.
        return Err(format!(
            "segment {:?} names no key component (a segment is never empty, `.`, `..` or \
             `\\`-bearing)",
            glob_echo(seg)
        ));
    }
    if glob.split('/').count() > crate::glob::MAX_PATTERN_SEGMENTS {
        return Err(format!(
            "exceeds {} segments",
            crate::glob::MAX_PATTERN_SEGMENTS
        ));
    }
    // The LENGTH of one segment, not only how many: the matcher tries a
    // segment against every path component of every file, so an
    // unbounded one buys unbounded work per file from inside the
    // manifest cap. Its bytes are COUNTED, never echoed — a 250 KB
    // segment must not ride the finding.
    if let Some(seg) = glob
        .split('/')
        .find(|seg| seg.len() > crate::glob::MAX_PATTERN_SEGMENT_BYTES)
    {
        return Err(format!(
            "a segment of {} bytes exceeds the {}-byte bound on one segment",
            seg.len(),
            crate::glob::MAX_PATTERN_SEGMENT_BYTES
        ));
    }
    Ok(())
}

/// The binding's `layers:` grant from its block (RFC 0019 plan item 4).
/// The block's shape is the meta-schema's finding; these are the
/// loader's OWN rules for it, refused at the item under NML2081: the
/// matcher's glob rules over `allowRefs` and `denyRefs` (an over-cap
/// deny would be fail-open at match time), `maxStackDepth` at most the
/// language cap ([`nml_core::layers::MAX_STACK_DEPTH`]; whole and at
/// least 1 is the meta-schema's rule),
/// and `denyRefs` beside an EMPTY `allowRefs` — a veto with nothing to
/// veto, a declaration that can only mislead (an empty allowlist
/// already denies every ref). `None` without the block.
fn extract_layers(body: &Body, binding: &str) -> Result<Option<LayerGrant>, Box<Diagnostic>> {
    let Some(block) = body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) if nb.name.name == "layers" => Some(nb),
        _ => None,
    }) else {
        return Ok(None);
    };
    let grant = |message: String, span: Span| rule(codes::LAYER_GRANT_RULE, message, Some(span));
    let lists = BlockQuery::Found(&block.body);
    let refs = |key: &str| -> Result<Vec<String>, Box<Diagnostic>> {
        let mut out = Vec::new();
        for (i, item) in manifest_list(&lists, key)?
            .map(|list| list.items)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            let glob = item.text;
            if let Err(why) = glob_rule(glob) {
                return Err(grant(
                    format!(
                        "validator '{binding}' layers.{key}[{i}] = {:?}: {why}",
                        glob_echo(glob)
                    ),
                    item.span,
                ));
            }
            out.push(glob.to_string());
        }
        Ok(out)
    };
    let allow_refs = refs("allowRefs")?;
    let deny_refs = refs("denyRefs")?;
    if allow_refs.is_empty() && !deny_refs.is_empty() {
        let at = manifest_list(&lists, "denyRefs")?.map_or(block.name.span, |list| list.key);
        return Err(grant(
            format!(
                "validator '{binding}' layers.denyRefs has nothing to veto: allowRefs is empty \
                 (an empty allowlist already denies every ref)"
            ),
            at,
        ));
    }
    // Whole and at least 1 is the meta-schema's rule (`number(min = 1,
    // multipleOf = 1)`, judged before the loader runs — NML2088 at the
    // value); the loader's own is the language cap, compared exactly.
    let cap = nml_core::layers::MAX_STACK_DEPTH;
    let max_stack_depth = match body_property(&block.body, "maxStackDepth") {
        Some(value) => match &value.value {
            Value::Number(n) if *n <= i64::from(cap) => {
                n.to_u64().and_then(|n| u32::try_from(n).ok())
            }
            Value::Number(n) => {
                return Err(grant(
                    format!(
                        "validator '{binding}' layers.maxStackDepth = {n} exceeds the language \
                         cap {cap}"
                    ),
                    value.span,
                ));
            }
            _ => None,
        },
        None => None,
    };
    Ok(Some(LayerGrant {
        allow_refs,
        deny_refs,
        max_stack_depth,
    }))
}

fn extract_directives(decl: Option<&ArrayDecl>) -> Result<Vec<DirectiveDecl>, Box<Diagnostic>> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut named: HashMap<&str, Span> = HashMap::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(rule(
                codes::UNNAMED_ENTRY,
                "`[]directive` entries must be named items (`- name:`)".to_string(),
                Some(item.span),
            ));
        };
        // A directive is its name (`Vocabulary::get` answers by it): one
        // entry per name, as for a schema and a binding.
        name_once(&mut named, "directive", name)?;
        // RFC 0019: the language's merge-policy directives are reserved —
        // known under every vocabulary, never redeclared under a package's
        // own meaning (NML2082, at the entry).
        if nml_core::layers::is_builtin_directive(&name.name) {
            return Err(rule(
                codes::RESERVED_DIRECTIVE,
                format!(
                    "`[]directive` entry '{0}' redeclares the language's merge-policy \
                     directive `#{0}` — `sealed`, `identity`, `append` and `overlay` are \
                     reserved (RFC 0019) and known under every vocabulary; rename it",
                    name.name
                ),
                Some(name.span),
            ));
        }
        let arg_raw = body_property_string(body, "arg").ok_or_else(|| {
            rule(
                codes::MISSING_REQUIRED_FIELD,
                format!("`[]directive` entry '{}' is missing `arg`", name.name),
                Some(name.span),
            )
        })?;
        let arg = DirectiveArg::parse(&arg_raw).ok_or_else(|| {
            rule(
                codes::INVALID_ENUM_VALUE,
                format!(
                    "directive '{}' has unknown arg kind '{arg_raw}' (none|ident|string|number)",
                    name.name
                ),
                Some(name.span),
            )
        })?;
        let doc = body_property_string(body, "doc").ok_or_else(|| {
            rule(
                codes::MISSING_REQUIRED_FIELD,
                format!("`[]directive` entry '{}' is missing `doc`", name.name),
                Some(name.span),
            )
        })?;
        out.push(DirectiveDecl {
            name: name.name.clone(),
            arg,
            doc,
        });
    }
    Ok(out)
}

fn extract_membership(body: &Body) -> Result<MembershipSemantics, Box<Diagnostic>> {
    let lists = BlockQuery::Found(body);
    Ok(MembershipSemantics {
        member_keywords: strings(manifest_list(&lists, "memberKeywords")?),
        builtin_refs: strings(manifest_list(&lists, "builtinRefs")?),
        user_ref_prefix: body_property_string(body, "userRefPrefix"),
    })
}

/// A manifest's list-valued entry through the language's one accessor
/// ([`BlockQuery::string_list`]) and the loader's ONE gate over it: an
/// element that is a template string (`"tenants/{{x}}/**"`) is refused at
/// the element (NML2104, the `cause` of the manifest's NML2088 row). The
/// meta-schema admits a template as a `string`, so it
/// reached the loader, where the accessor set it aside — and every list
/// here names files, keys, units or markers, plain text, so an element
/// read as nothing was a silently narrower `files`, a dropped unit or,
/// in `denyRefs`, a veto that loaded and never fired (fail-open, the
/// class NML2081 refuses for a glob no key can match). `None` for an
/// absent entry. Every list read in this module comes through here.
fn manifest_list<'a>(
    lists: &BlockQuery<'a>,
    key: &str,
) -> Result<Option<StringList<'a>>, Box<Diagnostic>> {
    let Some(list) = lists.string_list(key) else {
        return Ok(None);
    };
    if let Some(at) = list.templates.first() {
        return Err(rule(
            codes::TEMPLATE_IN_LIST,
            format!(
                "`{key}` holds a template string (`{{{{…}}}}`) — a manifest list holds plain \
                 string literals, and a template names no file, key, unit or marker; spell \
                 a literal `{{{{` as `\\u{{7B}}{{`"
            ),
            Some(*at),
        ));
    }
    Ok(Some(list))
}

/// The owned strings of a list read through [`manifest_list`]; none for
/// an absent entry.
fn strings(list: Option<StringList<'_>>) -> Vec<String> {
    list.map(|list| {
        list.items
            .iter()
            .map(|item| item.text.to_string())
            .collect()
    })
    .unwrap_or_default()
}

fn string_value(v: &SpannedValue) -> Option<String> {
    match &v.value {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// The string property `key` of a body.
fn body_property_string(body: &Body, key: &str) -> Option<String> {
    body_property(body, key).and_then(string_value)
}

/// The property `key` of a body, with its value's span.
fn body_property<'b>(body: &'b Body, key: &str) -> Option<&'b SpannedValue> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == key => Some(&p.value),
        _ => None,
    })
}

/// The boolean property `key` of a body.
fn body_property_bool(body: &Body, key: &str) -> Option<bool> {
    body_property(body, key).and_then(|v| match &v.value {
        Value::Bool(b) => Some(*b),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"// test package
package nudge:
    version = "0.1.0"
    formatVersion = 1
    rootMarkers:
        - "nudge.nml"
    modifiers:
        - "allow"
        - "deny"
    membership:
        memberKeywords:
            - "role"
            - "plan"
        builtinRefs:
            - "@public"
        userRefPrefix = "@user/"

[]schema schemas:
    - server:
        file = "server.model.nml"
    - denial:
        file = "denial.model.nml"

[]validator validators:
    - server:
        files:
            - "nudge.nml"
            - "nudge.server.nml"
        schemas:
            - server
            - denial
        strict = true

[]directive directives:
    - live:
        arg = "none"
        doc = "Hot-reloadable."
"#;

    const SERVER_SCHEMA: &str = "\
enum sameSite:
    - \"Lax\"
    - \"Strict\"

model server:
    name string+
    cookieSameSite sameSite?
    denial denial?
";

    const DENIAL_SCHEMA: &str = "\
model denial:
    name string+
    title string
";

    fn resolve(file: &str) -> Result<String, String> {
        match file {
            "server.model.nml" => Ok(SERVER_SCHEMA.to_string()),
            "denial.model.nml" => Ok(DENIAL_SCHEMA.to_string()),
            other => Err(format!("no such file {other}")),
        }
    }

    fn package() -> SchemaPackage {
        SchemaPackage::from_parts(MANIFEST, resolve).expect("package loads")
    }

    /// RFC 0030 meta-validation: exact subsumption drives the shadowed-
    /// binding warning; non-shadowed orders stay quiet.
    /// r88 P11 (spike): the load-time rules of `budgetUnits` — syntax,
    /// and never narrower than inference.
    #[test]
    fn budget_units_are_validated_at_load() {
        let binding = |globs: &[&str]| ValidatorBinding {
            name: "b".to_string(),
            files: globs.iter().map(|g| g.to_string()).collect(),
            schemas: vec![],
            strict: false,
            layers: None,
            span: Span::new(0, 0),
            file_spans: globs.iter().map(|_| Span::new(0, 0)).collect(),
        };
        let ok = |units: &[&str], globs: &[&str]| {
            validate_budget_units(
                &units.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                &[binding(globs)],
            )
        };
        assert!(ok(&["tenants/*"], &["tenants/**/*.flow.nml"]).is_ok());
        assert!(ok(&["tenants/*"], &["tenants/*/flows/**"]).is_ok());
        assert!(
            ok(&["tenants"], &["tenants/**"]).is_ok(),
            "shallower: allowed"
        );
        assert!(ok(&["orgs/*"], &["orgs/*/tenants/**"]).is_ok());
        assert!(
            ok(&["tenants/*"], &["admin/ops.flow.nml"]).is_ok(),
            "wildcard-free: unconstrained"
        );
        assert!(
            ok(&[], &["tenants/**"]).is_ok(),
            "no declaration: inference"
        );
        let narrower = ok(&["tenants/*/flows"], &["tenants/**"]).unwrap_err();
        assert!(
            narrower.contains("in the root unit") && narrower.contains("`tenants/*`"),
            "{narrower}"
        );
        let sideways = ok(&["orgs/*/tenants/*"], &["orgs/*/**"]).unwrap_err();
        assert!(sideways.contains("in the root unit"), "{sideways}");
        let literal = ok(&["tenants/cu"], &["tenants/**"]).unwrap_err();
        assert!(
            literal.contains("in the root unit"),
            "a literal covers one tenant only: {literal}"
        );
        let deep = ok(&["*/tenants/*"], &["**/tenants/*/**"]).unwrap_err();
        assert!(deep.contains("every depth"), "{deep}");
        // RFC 0026 B-3: a unit nesting inside another without pinning a
        // wildcard of it is refused, both units named; a pinned nesting
        // (E38's root catch-all beside the tenants) stands.
        for (units, globs) in [
            (&["tenants/*", "tenants/*/*"][..], &["tenants/**"][..]),
            (&["tenants/*/*", "tenants/*"], &["tenants/**"]),
            (&["tenants/cu", "tenants/*/*"], &["tenants/cu/**"]),
            (&["orgs/*", "orgs/*/tenants/*"], &["orgs/**"]),
            (&["tenants/*", "tenants/t-*/x"], &["tenants/**"]),
        ] {
            let nested = ok(units, globs).unwrap_err();
            assert!(
                nested.contains("nests inside unit") && nested.contains("pin the inner unit"),
                "{units:?}: {nested}"
            );
        }
        assert!(ok(&["*", "tenants/*"], &["**/*.model.nml", "tenants/**"]).is_ok());
        assert!(ok(&["tenants/*", "tenants/ops/*"], &["tenants/**"]).is_ok());
        assert!(ok(&["tenants/*", "vendor/*"], &["tenants/**", "vendor/**"]).is_ok());
        for bad in [
            &["**"][..],
            &["tenants/**"],
            &[""],
            &["tenants//x"],
            &["tenants/.."],
        ] {
            assert!(ok(bad, &["tenants/**"]).is_err(), "{bad:?}");
        }
        // The syntax refusals name the rule: a `.` segment and a `**`
        // segment are "one depth" refusals; an empty pattern and one
        // past `MAX_PATTERN_SEGMENTS` are "not a directory pattern".
        let dot = ok(&["tenants/./x"], &["tenants/**"]).unwrap_err();
        assert!(
            dot.contains("segment \".\"") && dot.contains("a unit is a directory at ONE depth"),
            "{dot}"
        );
        let star_star = ok(&["tenants/**"], &["tenants/**"]).unwrap_err();
        assert!(
            star_star.contains("a unit is a directory at ONE depth"),
            "{star_star}"
        );
        let empty = ok(&[""], &["tenants/**"]).unwrap_err();
        assert!(empty.contains("is not a directory pattern"), "{empty}");
        let sixty_five = vec!["a"; crate::glob::MAX_PATTERN_SEGMENTS + 1].join("/");
        let too_deep = ok(&[sixty_five.as_str()], &["tenants/**"]).unwrap_err();
        assert!(
            too_deep.contains("is not a directory pattern")
                && too_deep.contains(&format!(
                    "1 to {} `/`-separated",
                    crate::glob::MAX_PATTERN_SEGMENTS
                )),
            "{too_deep}"
        );
        let sixty_four = vec!["a"; crate::glob::MAX_PATTERN_SEGMENTS].join("/");
        assert!(
            ok(&[sixty_four.as_str()], &["admin/x.nml"]).is_ok(),
            "exactly the bound is a pattern"
        );
    }

    /// r103-sec F3: the loader refuses a glob segment past
    /// [`crate::glob::MAX_PATTERN_SEGMENT_BYTES`] in every glob
    /// vocabulary — `files`, `allowRefs`, `denyRefs`, `budgetUnits` —
    /// because past it the matcher answers `false` for every path, which
    /// is FAIL-OPEN for a veto. And the finding does not carry the glob
    /// whole: a 250 KB segment rode the `message` and the
    /// `cause.message` verbatim, so the refusal elides its middle around
    /// the exact byte count ([`MAX_GLOB_ECHO_BYTES`]), cut on character
    /// boundaries.
    #[test]
    fn an_over_long_glob_segment_is_refused_and_never_echoed_whole() {
        let huge = "*a".repeat(125_000);
        assert!(huge.len() > 200_000);
        let why = glob_rule(&format!("tenants/{huge}")).unwrap_err();
        assert!(
            why.contains(&format!(
                "a segment of {} bytes exceeds the {}-byte bound",
                huge.len(),
                crate::glob::MAX_PATTERN_SEGMENT_BYTES
            )),
            "{why}"
        );
        assert!(
            !why.contains(&huge),
            "the rule's own sentence must not carry it"
        );
        // Exactly the bound loads (`*a` pairs: a run of bare `*` would
        // trip the whole-segment `**` rule first).
        let at = "*a".repeat(crate::glob::MAX_PATTERN_SEGMENT_BYTES / 2);
        assert_eq!(at.len(), crate::glob::MAX_PATTERN_SEGMENT_BYTES);
        assert!(glob_rule(&format!("tenants/{at}")).is_ok());

        // The speller: short globs verbatim, a long one elided around
        // its exact size, never sliced mid-character.
        assert_eq!(glob_echo("tenants/**"), "tenants/**");
        let echoed = glob_echo(&huge);
        assert!(echoed.len() < 4 * MAX_GLOB_ECHO_BYTES, "{}", echoed.len());
        assert!(
            echoed.contains(&format!("({} bytes)", huge.len())),
            "{echoed}"
        );
        let wide = "\u{1f600}".repeat(MAX_GLOB_ECHO_BYTES);
        let echoed = glob_echo(&wide);
        assert!(echoed.len() < 4 * MAX_GLOB_ECHO_BYTES, "{}", echoed.len());
        assert!(
            echoed.contains(&format!("({} bytes)", wide.len())),
            "{echoed}"
        );

        // Through the manifest, in all four vocabularies: the refusal is
        // NML2081 and the row stays small. `allowRefs` and `denyRefs`
        // are the two that MUST be here — past the bound the matcher
        // answers `false` for every path, which is fail-closed for an
        // allow and FAIL-OPEN for a veto, so a `denyRefs` entry the
        // loader waved through would be a grant that vetoes nothing.
        let seg = "*a".repeat(600); // 1200 bytes, past the 1 KiB bound
        let layered = |block: &str| {
            MANIFEST.replace(
                "        strict = true\n",
                &format!("        strict = true\n        layers:\n{block}"),
            )
        };
        for (what, text) in [
            (
                "files",
                MANIFEST.replace("\"nudge.nml\"", &format!("\"tenants/{seg}\"")),
            ),
            (
                "budgetUnits",
                MANIFEST.replace("\"nudge.nml\"", "\"tenants/**\"").replace(
                    "    formatVersion = 1\n",
                    &format!(
                        "    formatVersion = 1\n    budgetUnits:\n        - \"tenants/{seg}\"\n"
                    ),
                ),
            ),
            (
                "allowRefs",
                layered(&format!(
                    "            allowRefs:\n                - \"vendor/{seg}\"\n"
                )),
            ),
            (
                "denyRefs",
                layered(&format!(
                    "            allowRefs:\n                - \"vendor/**\"\n            \
                     denyRefs:\n                - \"vendor/{seg}\"\n"
                )),
            ),
        ] {
            let err = SchemaPackage::from_parts(&text, resolve).unwrap_err();
            let PackageError::Manifest { errors, .. } = &err else {
                panic!("{what}: {err:?}");
            };
            let message = &errors[0].message;
            assert!(
                message.contains("exceeds the 1024-byte bound"),
                "{what}: {message}"
            );
            assert!(
                message.len() < 4 * MAX_GLOB_ECHO_BYTES,
                "{what}: the row carried {} bytes",
                message.len()
            );
        }
    }

    /// r105-cov: the bound and the speller at their exact EDGES, and the
    /// one arm that ran before the bound. A `budgetUnits` segment of
    /// exactly the bound loads (the `>=` mutant was caught for `files`
    /// alone); a glob of exactly `MAX_GLOB_ECHO_BYTES` is echoed verbatim;
    /// the elision's TAIL cut lands on a character boundary too (the
    /// four-byte fixture put that cut on a boundary by arithmetic luck —
    /// 640 − 40 is a multiple of 4 — so dropping the tail's boundary walk
    /// survived); and the "names no key component" arm, which runs
    /// BEFORE the byte bound, spells its segment through the same
    /// elision, so a `\`-bearing segment of 200 KB no longer rides the
    /// row whole (it did: `{seg:?}` verbatim, twice on the wire).
    #[test]
    fn the_glob_bound_and_its_echo_hold_at_their_exact_edges() {
        let at = "*a".repeat(crate::glob::MAX_PATTERN_SEGMENT_BYTES / 2);
        let text = MANIFEST.replace(
            "    formatVersion = 1\n",
            &format!("    formatVersion = 1\n    budgetUnits:\n        - \"tenants/{at}\"\n"),
        );
        let loaded = SchemaPackage::from_parts(&text, resolve)
            .expect("a `budgetUnits` segment AT the bound loads");
        assert_eq!(loaded.manifest.budget_units, [format!("tenants/{at}")]);

        // The speller's edges: verbatim AT the echo bound; past it, cut
        // on character boundaries at BOTH ends.
        let exact = "a".repeat(MAX_GLOB_ECHO_BYTES);
        assert_eq!(glob_echo(&exact), exact);
        let three = "\u{20ac}".repeat(MAX_GLOB_ECHO_BYTES + 1);
        assert_eq!(three.len(), 3 * (MAX_GLOB_ECHO_BYTES + 1));
        assert!(
            !three.is_char_boundary(MAX_GLOB_ECHO_BYTES * 2 / 3)
                && !three.is_char_boundary(three.len() - MAX_GLOB_ECHO_BYTES / 4),
            "the fixture must put BOTH cuts inside a character"
        );
        let echoed = glob_echo(&three);
        assert!(
            echoed.contains(&format!("({} bytes)", three.len())),
            "{echoed}"
        );
        assert!(
            echoed
                .chars()
                .all(|c| c == '\u{20ac}' || "…() bytes0123456789".contains(c)),
            "{echoed}"
        );

        // The not-plain arm: a `\`-bearing segment past the echo bound
        // is counted, never echoed — standing alone and through the
        // manifest (the NML string spells the backslash as `\\`).
        let huge = format!("{}\\x", "a".repeat(200_000));
        let why = glob_rule(&format!("tenants/{huge}")).unwrap_err();
        assert!(why.contains("names no key component"), "{why}");
        assert!(why.contains(&format!("({} bytes)", huge.len())), "{why}");
        assert!(
            why.len() < 4 * MAX_GLOB_ECHO_BYTES,
            "the arm's sentence carried {} bytes",
            why.len()
        );
        let text = MANIFEST.replace(
            "\"nudge.nml\"",
            &format!("\"tenants/{}\\\\x\"", "a".repeat(200_000)),
        );
        let err = SchemaPackage::from_parts(&text, resolve).unwrap_err();
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(
            errors[0].message.contains("names no key component"),
            "{}",
            errors[0].message
        );
        assert!(
            errors[0].message.len() < 4 * MAX_GLOB_ECHO_BYTES,
            "the row carried {} bytes",
            errors[0].message.len()
        );
    }

    /// A `budgetUnits` declaration the loader refuses is refused AT the
    /// `budgetUnits` block (the manifest loads as NML2088 with that
    /// location), never at the package name: the operator is sent to the
    /// declaration they wrote.
    #[test]
    fn a_refused_budget_units_declaration_is_spanned_at_its_block() {
        let text = MANIFEST.replace("\"nudge.nml\"", "\"tenants/**\"").replace(
            "    formatVersion = 1\n",
            "    formatVersion = 1\n    budgetUnits:\n        - \"tenants/*/flows\"\n",
        );
        let err = SchemaPackage::from_parts(&text, resolve).unwrap_err();
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        let message = &errors[0].message;
        assert!(message.contains("in the root unit"), "{message}");
        let span = errors[0].span.expect("spanned");
        assert_eq!(&text[span.start..span.end], "budgetUnits", "{span:?}");
        // A loader rule's finding is LOCATED like a meta-validation
        // finding: the row names the line and column of the block.
        let at = at.as_ref().expect("located");
        assert_eq!((at.line, at.column), (5, 5), "{at}");
        assert!(
            err.to_string()
                .starts_with("manifest failed validation at 5:5: budgetUnits"),
            "{err}"
        );
    }

    #[test]
    fn shadow_warnings_fire_on_fully_shadowed_bindings() {
        let shadowed = MANIFEST.replace(
            "[]directive directives:",
            "    - narrow:\n        files:\n            - \"nudge.nml\"\n        schemas:\n            - server\n\n[]directive directives:",
        );
        let p = SchemaPackage::from_parts(&shadowed, resolve).unwrap();
        let warnings = p.manifest.shadow_warnings();
        assert!(
            warnings.iter().any(|w| w
                .message
                .contains("'narrow' is fully shadowed by earlier validator 'server'")
                && w.span.is_some()),
            "{warnings:?}"
        );
        assert!(package().manifest.shadow_warnings().is_empty());
    }

    /// The budget is SHARED across the pairs, so an analysis whose
    /// early comparisons are expensive has nothing left for its late
    /// ones — which is the property, stated where it costs no wall
    /// clock to state.
    ///
    /// The fixture: three MAXIMAL globs (64 segments of 1 KiB, every
    /// byte inside the published pattern bounds), then a `**` binding
    /// that shadows a dead one at the end. ONE maximal pair charges the
    /// whole budget, so the analysis reaches the last pair with nothing
    /// and the advisory is withheld — where the unbudgeted comparison,
    /// asserted here so the fixture cannot go vacuous, finds it. Both
    /// halves of the bound have a pin: this one that it BITES, and
    /// `a_realistic_manifest_never_reaches_the_shadow_budget` that it
    /// does not bite a manifest anyone would write.
    #[test]
    fn the_shadow_budget_is_shared_across_the_pairs_it_compares() {
        let maximal = |i: usize| {
            format!(
                "v{i}/{}",
                vec!["*x".repeat(512); crate::glob::MAX_PATTERN_SEGMENTS - 1].join("/")
            )
        };
        let bindings: String = (0..3)
            .map(|i| {
                let g = maximal(i);
                format!(
                    "    - v{i}:\n        files:\n            - \"{g}\"\n        schemas:\n            - server\n"
                )
            })
            .collect();
        let text = MANIFEST.replace(
            "[]directive directives:",
            &format!(
                "{bindings}    - wide:\n        files:\n            - \"**\"\n        schemas:\n            - server\n    - dead:\n        files:\n            - \"docs/readme.nml\"\n        schemas:\n            - server\n\n[]directive directives:"
            ),
        );
        let p = SchemaPackage::from_parts(&text, resolve).expect("loads");
        // Not vacuous: unbudgeted, `wide` does shadow `dead`.
        assert!(crate::glob::subsumes("**", "docs/readme.nml"));
        let warnings = p.manifest.shadow_warnings();
        assert!(
            warnings.is_empty(),
            "the budget is shared across pairs: three maximal globs spend it, so the last \
             pair is never compared — got {warnings:?}"
        );
    }

    /// The wall clock, on the perf tier where an absolute belongs.
    ///
    /// A manifest of N bindings makes O(N²) glob comparisons, and the
    /// per-comparison bound bounds none of them together. A manifest
    /// inside every published bound — 200 bindings, each glob one the
    /// subset construction explodes on — used to take minutes here (447
    /// bindings in a 64 KiB manifest measured 108 s, and 127 s for one
    /// `textDocument/diagnostic` on it in the language server, which
    /// runs this per pull on a manifest a REPOSITORY wrote). Two shapes,
    /// because the cost has two factors and a budget counted in the
    /// wrong currency bounds only one of them: MANY small explosive
    /// globs (the pair count), and a FEW maximal ones (the per-state
    /// work — one such pair alone cost 8 s).
    #[test]
    #[ignore = "perf tier: run with `cargo test -p nml-validate --release --lib -- --ignored perf_` \
                (release-timed; the default lane pins the budget, not the clock)"]
    fn perf_the_shadow_analysis_is_bounded_across_every_pair_it_compares() {
        let small = |i: usize| format!("{}{i}", "*x".repeat(32));
        let maximal = |i: usize| {
            format!(
                "v{i}/{}",
                vec!["*x".repeat(512); crate::glob::MAX_PATTERN_SEGMENTS - 1].join("/")
            )
        };
        type Shape<'a> = (&'a str, usize, &'a dyn Fn(usize) -> String);
        let shapes: [Shape<'_>; 2] = [("many small", 200, &small), ("few maximal", 3, &maximal)];
        for (label, count, glob) in shapes {
            let bindings: String = (0..count)
                .map(|i| {
                    let g = glob(i);
                    format!(
                        "    - v{i}:\n        files:\n            - \"{g}\"\n        schemas:\n            - server\n"
                    )
                })
                .collect();
            let text = MANIFEST.replace(
                "[]directive directives:",
                &format!("{bindings}\n[]directive directives:"),
            );
            let p = SchemaPackage::from_parts(&text, resolve).expect("loads");
            assert!(p.manifest.validators.len() > count, "{label}: bindings");
            let start = std::time::Instant::now();
            let warnings = p.manifest.shadow_warnings();
            let elapsed = start.elapsed();
            // Generous on purpose: the quantity under test is an ORDER
            // OF MAGNITUDE, not a millisecond count, and the values it
            // replaces are 108 s for 447 bindings and minutes at the
            // manifest's own byte bound. Measured here: 0.5 s and 0.2 s.
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "{label}: the shadow analysis must be bounded across pairs, not only \
                 within one: {elapsed:?}"
            );
            // The budget withholds advisories; it never invents one.
            assert!(warnings.is_empty(), "{label}: {warnings:?}");
        }
    }

    /// The budget is calibrated so a REALISTIC manifest never reaches
    /// it: over a manifest with far more bindings than any real one, the
    /// budgeted analysis answers exactly what the UNBUDGETED comparison
    /// does, pair for pair. A budget a working manifest hits would
    /// silently stop reporting dead bindings, which is the one thing
    /// this bound could cost.
    #[test]
    fn a_realistic_manifest_never_reaches_the_shadow_budget() {
        let bindings: String = (0..60)
            .map(|i| {
                format!(
                    "    - v{i}:\n        files:\n            - \"tenants/t{i}/**/*.flow.nml\"\n            - \"shared/t{i}/**/*.nml\"\n        schemas:\n            - server\n"
                )
            })
            .collect();
        // The shadower is the LAST earlier binding and nothing before it
        // subsumes the dead one, so the analysis must compare every
        // pair before it can report — the position an exhausted budget
        // loses the advisory at.
        let text = MANIFEST.replace(
            "[]directive directives:",
            &format!(
                "{bindings}    - wide:\n        files:\n            - \"**\"\n        schemas:\n            - server\n    - dead:\n        files:\n            - \"docs/readme.nml\"\n        schemas:\n            - server\n\n[]directive directives:"
            ),
        );
        let p = SchemaPackage::from_parts(&text, resolve).expect("loads");
        let warnings = p.manifest.shadow_warnings();
        assert!(
            warnings.iter().any(|w| w
                .message
                .contains("'dead' is fully shadowed by earlier validator 'wide'")),
            "the budget must not withhold a real manifest's advisory: {warnings:?}"
        );
        // Pair for pair with the UNBUDGETED comparison: same shadow
        // relation, so the budget changed no answer on this manifest.
        let unbudgeted: Vec<(String, String)> =
            p.manifest
                .validators
                .iter()
                .enumerate()
                .filter_map(|(i, later)| {
                    p.manifest.validators[..i]
                        .iter()
                        .find(|earlier| {
                            later.files.iter().all(|lg| {
                                earlier.files.iter().any(|eg| crate::glob::subsumes(eg, lg))
                            })
                        })
                        .map(|earlier| (later.name.clone(), earlier.name.clone()))
                })
                .collect();
        assert_eq!(
            warnings.len(),
            unbudgeted.len(),
            "budgeted {warnings:?} vs unbudgeted {unbudgeted:?}"
        );
        for (later, earlier) in &unbudgeted {
            assert!(
                warnings.iter().any(|w| w.message.contains(&format!(
                    "'{later}' is fully shadowed by earlier validator '{earlier}'"
                ))),
                "budgeted analysis lost {later} ← {earlier}: {warnings:?}"
            );
        }
        // And the HEADROOM, in the bound's own units: the analysis'
        // loop, run again over its own budget, so what it SPENT is a
        // number this test can assert on. The agreement above is the
        // verdict; this is why it holds, and it is what a future
        // tightening of MAX_SHADOW_WORK would break first.
        let mut budget = MAX_SHADOW_WORK;
        for (i, later) in p.manifest.validators.iter().enumerate() {
            let _ = p.manifest.validators[..i].iter().find(|earlier| {
                later.files.iter().all(|lg| {
                    earlier
                        .files
                        .iter()
                        .any(|eg| crate::glob::subsumes_budgeted(eg, lg, &mut budget))
                })
            });
        }
        let spent = MAX_SHADOW_WORK - budget;
        assert!(
            spent * 4 < MAX_SHADOW_WORK,
            "a manifest of {} bindings spent {spent} of {MAX_SHADOW_WORK}: the budget is \
             too tight to leave a real one's advisories alone",
            p.manifest.validators.len(),
        );
    }

    /// r89 (P11): NML2092 fires under INFERENCE for the E38 loud shapes
    /// only — spanned at the glob, with the declaration that silences it
    /// spelled both ways — and an explicit `budgetUnits` silences it;
    /// a glob whose only wildcard run is its last has no gap.
    #[test]
    fn budget_unit_gaps_name_the_loud_shapes_at_the_glob() {
        let with_glob = |glob: &str, units: &str| {
            let text = MANIFEST
                .replace("\"nudge.nml\"", &format!("{glob:?}"))
                .replace(
                    "    formatVersion = 1\n",
                    &format!("    formatVersion = 1\n{units}"),
                );
            SchemaPackage::from_parts(&text, resolve).unwrap()
        };
        let p = with_glob("tenants/*/flows/**", "");
        let gaps = crate::workspace::budget_unit_gaps(&p.manifest);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        let gap = &gaps[0];
        assert_eq!(gap.code, Some(nml_core::diagnostic::codes::BUDGET_UNIT_GAP));
        assert_eq!(gap.severity, nml_core::diagnostic::Severity::Warning);
        assert!(
            gap.message.contains("files[0] = \"tenants/*/flows/**\"")
                && gap
                    .message
                    .contains("the inferred budget unit is `tenants/*/flows/*`")
                && gap
                    .message
                    .contains("declare budgetUnits = [\"tenants/*\"]")
                && gap.message.contains("or [\"tenants/*/flows/*\"]"),
            "{}",
            gap.message
        );
        // The span is the glob's own string literal.
        let text = MANIFEST.replace("\"nudge.nml\"", "\"tenants/*/flows/**\"");
        let span = gap.span.expect("spanned at the glob");
        assert_eq!(&text[span.start..span.end], "\"tenants/*/flows/**\"");
        // A `**` before the boundary: no fixed depth to declare.
        let p = with_glob("**/tenants/*/**", "");
        let gaps = crate::workspace::budget_unit_gaps(&p.manifest);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(
            gaps[0].message.contains("no fixed depth")
                && gaps[0].message.contains("(`*`, not `**`)"),
            "{}",
            gaps[0].message
        );
        // Declared: silent. Last-run-only shapes: silent.
        let p = with_glob(
            "tenants/*/flows/**",
            "    budgetUnits:\n        - \"tenants/*\"\n",
        );
        assert!(crate::workspace::budget_unit_gaps(&p.manifest).is_empty());
        for quiet in [
            "tenants/**/*.flow.nml",
            "tenants/*/flows/*.flow.nml",
            "**",
            "admin/x.nml",
        ] {
            let p = with_glob(quiet, "");
            assert!(
                crate::workspace::budget_unit_gaps(&p.manifest).is_empty(),
                "{quiet}"
            );
        }
    }

    #[test]
    fn manifest_parses_to_typed_form() {
        let p = package();
        let m = &p.manifest;
        assert_eq!(m.name, "nudge");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.format_version, 1);
        assert_eq!(m.root_markers, ["nudge.nml"]);
        assert_eq!(m.modifiers, ["allow", "deny"]);
        assert_eq!(m.membership.member_keywords, ["role", "plan"]);
        assert_eq!(m.membership.builtin_refs, ["@public"]);
        assert_eq!(m.membership.user_ref_prefix.as_deref(), Some("@user/"));
        assert_eq!(m.schemas.len(), 2);
        assert_eq!(m.validators.len(), 1);
        assert_eq!(m.directives.len(), 1);
        assert_eq!(m.directives[0].arg, DirectiveArg::None);
        // `doc` follows `arg` in the entry: the one property reader must
        // pick by KEY, not position (a mutant that took the first property
        // read "none" here and survived every other pin).
        assert_eq!(m.directives[0].doc, "Hot-reloadable.");
    }

    /// The parity-bearing path: a validator built from the package composes
    /// the named schema set, applies strictness + modifiers + membership, and
    /// produces boot-identical diagnostics (did-you-mean, unknown key,
    /// cross-schema refs resolving).
    /// A declared source carrying an NML2054 shape (an arm field named like
    /// the discriminator) fails the binding's validator build — the
    /// `Sources` refusal every front end renders as NML2091, the finding
    /// (its suggestion and note intact) as the row's note line.
    #[test]
    fn a_source_with_a_shadowed_discriminator_cannot_build_the_binding() {
        const MANIFEST: &str = "\
package shadow:
    version = \"0.1.0\"
    formatVersion = 1

[]schema schemas:
    - core:
        file = \"core.model.nml\"

[]validator validators:
    - flows:
        files:
            - \"*.flow.nml\"
        schemas:
            - core
";
        const CORE: &str = "\
model logEntry:
    kind string?

oneof record by kind:
    \"log\" -> logEntry
";
        let p = SchemaPackage::from_parts(MANIFEST, |file| match file {
            "core.model.nml" => Ok(CORE.to_string()),
            other => Err(format!("no such file {other}")),
        })
        .expect("the manifest loads; the source is judged at the binding");
        let binding = p.binding_for("a.flow.nml").expect("binding matches");
        let err = match p.validator(binding) {
            Ok(_) => panic!("the validator must not build over the shape"),
            Err(e) => e,
        };
        let PackageError::Sources { errors } = err else {
            panic!("the source-load refusal: {err}");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        let row = &errors[0];
        assert_eq!(
            row.code,
            Some(nml_core::diagnostic::codes::SHADOWED_DISCRIMINATOR)
        );
        // The loader names a source by its schema ENTRY (the manifest's
        // `core`), the vocabulary NML2091's row and note speak.
        assert_eq!(row.source.as_deref(), Some("core"));
        assert_eq!(
            row.suggestions.len(),
            1,
            "the deletion rides into the refusal"
        );
        assert_eq!(
            row.related.len(),
            1,
            "the union's note rides into the refusal"
        );
    }

    #[test]
    fn binding_validator_is_composed_strict_and_suggests() {
        let p = package();
        let binding = p.binding_for("nudge.nml").expect("binding matches");
        assert_eq!(binding.name, "server");
        let v = p.validator(binding).expect("validator builds");
        let file = nml_core::cst::parse_to_ast(
            "server main:\n    cookieSameSite = \"lax\"\n    unknownKey = 1\n    denial:\n        name = \"D\"\n        title = \"T\"\n",
        )
        .unwrap();
        let diags = v.validate(&file);
        assert!(
            diags
                .iter()
                .any(|d| d.rendered_message().contains("did you mean \"Lax\"")
                    && !d.suggestions.is_empty()),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.message.contains("unknownKey")
                && matches!(d.severity, nml_core::diagnostic::Severity::Error)),
            "strict unknown key must be an error: {diags:?}"
        );
        // The cross-schema composition resolves: `denial` (from the second
        // schema source) validates without unknown-model noise.
        assert!(
            !diags
                .iter()
                .any(|d| d.message.contains("denial") && d.message.contains("no model")),
            "{diags:?}"
        );
    }

    #[test]
    fn binding_glob_selection_is_first_match_and_root_relative() {
        let p = package();
        assert!(p.binding_for("nudge.server.nml").is_some());
        assert!(p.binding_for("app.nml").is_none());
        assert!(
            p.binding_for("sub/nudge.nml").is_none(),
            "globs are root-relative"
        );
    }

    /// The TEXT-SCAN fallback (manifests that fail to parse): the
    /// degradation gate must still fire precisely — including
    /// saturation for a formatVersion beyond u64 — instead of the
    /// parse-error wall.
    #[test]
    fn format_version_text_scan_gates_unparseable_manifests() {
        // Future syntax the current parser rejects, plus a readable version.
        let future = MANIFEST.replace(
            "formatVersion = 1",
            "formatVersion = 99\n    someFutureField ::= <<future-syntax>>",
        );
        match SchemaPackage::from_parts(&future, resolve) {
            Err(PackageError::UnsupportedFormatVersion {
                required: 99,
                supported,
            }) => {
                assert_eq!(supported, SUPPORTED_FORMAT_VERSION);
            }
            other => panic!("expected the precise gate, got {other:?}"),
        }
        // Beyond u64: the text scan saturates like the value path.
        let huge = MANIFEST.replace(
            "formatVersion = 1",
            &format!(
                "formatVersion = {}\n    someFutureField ::= <<future-syntax>>",
                "9".repeat(35)
            ),
        );
        match SchemaPackage::from_parts(&huge, resolve) {
            Err(PackageError::UnsupportedFormatVersion {
                required,
                supported,
            }) => {
                assert_eq!(required, u64::MAX);
                assert_eq!(supported, SUPPORTED_FORMAT_VERSION);
            }
            other => panic!("expected saturated gate, got {other:?}"),
        }
    }

    #[test]
    fn format_version_gate_precedes_meta_validation() {
        // Newer formatVersion + an unknown key that would meta-fail: the gate
        // must win, producing the one precise degradation error.
        let newer = MANIFEST.replace(
            "formatVersion = 1",
            "formatVersion = 99\n    someFutureField = \"x\"",
        );
        match SchemaPackage::from_parts(&newer, resolve) {
            Err(PackageError::UnsupportedFormatVersion {
                required: 99,
                supported,
            }) => {
                assert_eq!(supported, SUPPORTED_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    /// RFC 0016 made 34-digit integers parse (they used to die at
    /// NML0014), so a `formatVersion` beyond `u64` now reaches this code.
    /// It must still hit the degradation gate — not read as "missing
    /// formatVersion", and not fall through to a meta-validation wall.
    #[test]
    fn format_version_beyond_u64_still_hits_the_gate() {
        let huge = MANIFEST.replace(
            "formatVersion = 1",
            "formatVersion = 9999999999999999999999999999999999\n    someFutureField = \"x\"",
        );
        match SchemaPackage::from_parts(&huge, resolve) {
            Err(PackageError::UnsupportedFormatVersion { supported, .. }) => {
                assert_eq!(supported, SUPPORTED_FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedFormatVersion, got {other:?}"),
        }
    }

    #[test]
    fn meta_validation_rejects_unknown_keys_and_bad_arg_kinds() {
        let bad_key = MANIFEST.replace("version = \"0.1.0\"", "versio = \"0.1.0\"");
        assert!(matches!(
            SchemaPackage::from_parts(&bad_key, resolve),
            Err(PackageError::Manifest { .. })
        ));
        let bad_arg = MANIFEST.replace("arg = \"none\"", "arg = \"nonee\"");
        assert!(matches!(
            SchemaPackage::from_parts(&bad_arg, resolve),
            Err(PackageError::Manifest { .. })
        ));
    }

    /// r85 D5: a manifest finding names where it sits — `<line>:<col>` in
    /// the manifest text, `<file>:<line>:<col>` once the loader names the
    /// file — the place the reader opens next, in place of the old
    /// `(1 error(s))` count; a parse failure is located the same way.
    #[test]
    fn manifest_findings_are_located() {
        let bad_key = MANIFEST.replace("version = \"0.1.0\"", "versio = \"0.1.0\"");
        let err = SchemaPackage::from_parts(&bad_key, resolve).expect_err("meta-validation fails");
        let PackageError::Manifest { at: Some(at), .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!((at.file.as_deref(), at.line, at.column), (None, 3, 5));
        let text = err.to_string();
        assert!(
            text.starts_with("manifest failed validation at 3:5 (finding 1 of 2): "),
            "{text}"
        );
        assert!(!text.contains("error(s)"), "{text}");
        let named = err.clone().in_file("demo.package.nml").to_string();
        assert!(
            named.starts_with(
                "manifest failed validation at demo.package.nml:3:5 (finding 1 of 2): "
            ),
            "{named}"
        );
        // Every other variant passes through `in_file` unchanged.
        let missing = SchemaPackage::from_parts(MANIFEST, |_| Err::<String, _>("gone".to_string()))
            .expect_err("a source is missing");
        assert!(matches!(
            missing.in_file("demo.package.nml"),
            PackageError::MissingSource { .. }
        ),);
        let broken = format!("{MANIFEST}\n  oops\n");
        let err = SchemaPackage::from_parts(&broken, resolve).expect_err("parse fails");
        let PackageError::Manifest { at: Some(at), .. } = &err else {
            panic!("{err:?}");
        };
        assert!(at.line > 1, "{at}");
        assert!(
            err.to_string()
                .starts_with("manifest failed validation at "),
            "{err}"
        );
        // r86 (mutant MR27 survived): a parse failure is ONE finding, and
        // one finding carries no count at all — never `(finding 1 of 1)`.
        assert!(!err.to_string().contains("more)"), "{err}");
    }

    /// The spike's failing case, now closed at the loader level: a `[]schema`
    /// entry with no `file` is a precise load error, never a silent pass.
    #[test]
    fn schema_entry_missing_file_is_precise_error() {
        let broken = MANIFEST.replace(
            "    - server:\n        file = \"server.model.nml\"\n",
            "    - server:\n",
        );
        match SchemaPackage::from_parts(&broken, resolve) {
            Err(PackageError::Manifest { errors, .. }) => {
                // The lowering fix makes `- server:` a Named empty item, so
                // strict meta-validation reports the missing `file` itself;
                // the loader's own backstop would say `missing \`file\``.
                assert!(
                    errors
                        .iter()
                        .any(|e| e.message.contains("missing required field 'file'")
                            || e.message.contains("missing `file`")),
                    "{errors:?}"
                );
            }
            other => panic!("expected a precise missing-file error, got {other:?}"),
        }
    }

    #[test]
    fn validator_naming_undeclared_schema_is_rejected() {
        let broken = MANIFEST.replace("            - denial\n", "            - nonexistent\n");
        match SchemaPackage::from_parts(&broken, resolve) {
            Err(PackageError::Manifest { errors, .. }) => {
                let message = &errors[0].message;
                assert!(message.contains("nonexistent"), "{message}");
            }
            other => panic!("expected a manifest finding, got {other:?}"),
        }
    }

    #[test]
    fn package_name_charset_is_enforced() {
        let broken = MANIFEST.replace("package nudge:", "package Nudge:");
        assert!(matches!(
            SchemaPackage::from_parts(&broken, resolve),
            Err(PackageError::Manifest { .. })
        ));
    }

    #[test]
    fn missing_declared_source_names_the_file() {
        let broken = MANIFEST.replace("denial.model.nml", "gone.model.nml");
        match SchemaPackage::from_parts(&broken, resolve) {
            Err(PackageError::MissingSource { file, .. }) => assert_eq!(file, "gone.model.nml"),
            other => panic!("expected MissingSource, got {other:?}"),
        }
    }

    #[test]
    fn broken_source_reports_with_attribution() {
        let p = SchemaPackage::from_parts(MANIFEST, |f| match f {
            "server.model.nml" => Ok("model server:\n    @@@\n".to_string()),
            other => resolve(other),
        })
        .expect("load succeeds; source errors surface at validator build");
        let binding = p.binding_for("nudge.nml").unwrap();
        match p.validator(binding) {
            Err(PackageError::Sources { errors }) => {
                assert!(
                    errors.iter().any(|e| e.source.as_deref() == Some("server")),
                    "attribution to the logical source: {errors:?}"
                );
            }
            other => panic!("expected Sources error, got {other:?}"),
        }
    }

    /// Hash framing (RFC 0030): stable across reloads, changed by manifest
    /// edits AND by source edits, CRLF-insensitive, and boundary-unambiguous.
    #[test]
    fn content_hash_framing() {
        let base = package().content_hash();
        assert!(base.starts_with("blake3:"));
        assert_eq!(base, package().content_hash(), "deterministic");
        let manifest_edit = SchemaPackage::from_parts(
            &MANIFEST.replace("version = \"0.1.0\"", "version = \"0.2.0\""),
            resolve,
        )
        .unwrap()
        .content_hash();
        assert_ne!(base, manifest_edit, "manifest edits change the hash");
        let source_edit = SchemaPackage::from_parts(MANIFEST, |f| {
            resolve(f).map(|s| {
                if f == "denial.model.nml" {
                    s + "\n// edit\n"
                } else {
                    s
                }
            })
        })
        .unwrap()
        .content_hash();
        assert_ne!(base, source_edit, "source edits change the hash");
        let crlf =
            SchemaPackage::from_parts(MANIFEST, |f| resolve(f).map(|s| s.replace('\n', "\r\n")))
                .unwrap()
                .content_hash();
        assert_eq!(
            base, crlf,
            "LF-normalization makes line endings identity-neutral"
        );
    }

    /// The builtin meta package loads, binds `*.package.nml` at any depth,
    /// deliberately does not match a bare `package.nml`, and strictly
    /// validates manifests — including this module's own test manifest.
    #[test]
    fn builtin_meta_package_binds_and_validates_manifests() {
        let builtin = builtin_meta_package();
        assert_eq!(builtin.manifest.name, "nml");
        let binding = builtin
            .binding_for("nudge.package.nml")
            .expect("binds manifests");
        assert!(builtin.binding_for("sub/dir/other.package.nml").is_some());
        assert!(
            builtin.binding_for("package.nml").is_none(),
            "bare package.nml unmatched"
        );
        let v = builtin.validator(binding).expect("meta validator builds");
        let file = nml_core::cst::parse_to_ast(MANIFEST).unwrap();
        let errors: Vec<_> = v
            .validate(&file)
            .into_iter()
            .filter(|d| matches!(d.severity, nml_core::diagnostic::Severity::Error))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// RFC 0019 plan item 4 (RFC 0026 B-1): a validator's `layers:` block
    /// is ordinary schema on the meta-package and loads into the binding's
    /// grant — allow and deny globs in order, the stack cap when given —
    /// and a binding without the block carries none. An empty allowlist
    /// alone is a grant that denies every ref (RFC 0019's letter): it
    /// loads.
    #[test]
    fn a_layers_block_loads_into_the_bindings_grant() {
        let with = |block: &str| {
            MANIFEST.replace(
                "        strict = true\n",
                &format!("        strict = true\n        layers:\n{block}"),
            )
        };
        let p = SchemaPackage::from_parts(
            &with(
                "            allowRefs:\n                - \"vendor/**\"\n                - \
                 \"shared/lib/*.nml\"\n            denyRefs:\n                - \
                 \"vendor/vetoed/**\"\n            maxStackDepth = 4\n",
            ),
            resolve,
        )
        .unwrap();
        assert_eq!(
            p.manifest.validators[0].layers,
            Some(LayerGrant {
                allow_refs: vec!["vendor/**".to_string(), "shared/lib/*.nml".to_string()],
                deny_refs: vec!["vendor/vetoed/**".to_string()],
                max_stack_depth: Some(4),
            })
        );
        let p = SchemaPackage::from_parts(MANIFEST, resolve).unwrap();
        assert_eq!(p.manifest.validators[0].layers, None);
        let p = SchemaPackage::from_parts(&with("            allowRefs:\n"), resolve).unwrap();
        assert_eq!(
            p.manifest.validators[0].layers,
            Some(LayerGrant {
                allow_refs: vec![],
                deny_refs: vec![],
                max_stack_depth: None,
            })
        );
        // The cap itself is a legal cap.
        assert!(
            SchemaPackage::from_parts(
                &with("            allowRefs:\n                - \"vendor/**\"\n            maxStackDepth = 16\n"),
                resolve
            )
            .is_ok()
        );
    }

    /// The grant's OWN rules are NML2081 at the item (RFC 0019: "grant
    /// globs are meta-validated like `files` globs"; RFC 0026 B-1): a glob
    /// the matcher rejects (`**` inside a segment; over the segment cap —
    /// an over-cap deny would be fail-open at match time), `maxStackDepth`
    /// above the language cap, and `denyRefs` beside
    /// an empty `allowRefs` (a veto with nothing to veto). Each is ONE
    /// located manifest finding under the code, spanned at the offending
    /// item, so the row names its line and column. The block's SHAPE
    /// (`maxStackDepth = 0`, a missing `allowRefs`, an unknown property) is
    /// the meta-schema's finding — under the meta-schema's code at the
    /// loader, NML2088 at the universe.
    #[test]
    fn layers_grant_rules_are_nml2081_at_the_item() {
        let with = |block: &str| {
            MANIFEST.replace(
                "        strict = true\n",
                &format!("        strict = true\n        layers:\n{block}"),
            )
        };
        let refused = |text: &str, wants: &str, item: &str| {
            let err = SchemaPackage::from_parts(text, resolve).unwrap_err();
            let PackageError::Manifest { errors, at } = &err else {
                panic!("{err:?}");
            };
            assert_eq!(errors.len(), 1, "{errors:?}");
            let finding = &errors[0];
            assert_eq!(finding.code, Some(codes::LAYER_GRANT_RULE), "{finding:?}");
            assert!(finding.message.contains(wants), "{}", finding.message);
            let span = finding.span.expect("spanned at the item");
            assert_eq!(&text[span.start..span.end], item, "{finding:?}");
            assert!(at.is_some(), "located: {err}");
            assert!(
                err.to_string()
                    .starts_with("manifest failed validation at "),
                "{err}"
            );
        };
        refused(
            &with("            allowRefs:\n                - \"vendor/**x\"\n"),
            "validator 'server' layers.allowRefs[0] = \"vendor/**x\": `**` must be a whole segment",
            "\"vendor/**x\"",
        );
        let huge = (0..65).map(|_| "a").collect::<Vec<_>>().join("/");
        refused(
            &with(&format!(
                "            allowRefs:\n                - \"vendor/**\"\n            denyRefs:\n                - \"{huge}\"\n"
            )),
            "layers.denyRefs[0] = ",
            &format!("\"{huge}\""),
        );
        // A segment no key component can equal: a deny that never fires
        // (fail-open) and an allow that admits nothing, refused alike.
        for (glob, seg) in [
            ("vendor/./base.flow.nml", "."),
            ("vendor\\\\base.flow.nml", "vendor\\base.flow.nml"),
            ("/vendor/**", ""),
            ("vendor/x/../**", ".."),
            ("vendor/base.flow.nml/", ""),
        ] {
            refused(
                &with(&format!(
                    "            allowRefs:\n                - \"vendor/**\"\n            denyRefs:\n                - \"{glob}\"\n"
                )),
                &format!("layers.denyRefs[0] = \"{glob}\": segment {seg:?} names no key component"),
                &format!("\"{glob}\""),
            );
            refused(
                &with(&format!(
                    "            allowRefs:\n                - \"{glob}\"\n"
                )),
                &format!(
                    "layers.allowRefs[0] = \"{glob}\": segment {seg:?} names no key component"
                ),
                &format!("\"{glob}\""),
            );
        }
        // The same vocabulary for a `files` glob (the loader's own rule, NML2100).
        let files = MANIFEST.replace("\"nudge.server.nml\"", "\"nudge/./server.nml\"");
        assert_ne!(files, MANIFEST, "the files glob anchor");
        let err = SchemaPackage::from_parts(&files, resolve).unwrap_err();
        assert!(
            err.to_string()
                .contains("glob 'nudge/./server.nml': segment \".\" names no key component"),
            "{err}"
        );
        refused(
            &with(
                "            allowRefs:\n                - \"vendor/**\"\n            maxStackDepth = 17\n",
            ),
            "layers.maxStackDepth = 17 exceeds the language cap 16",
            "17",
        );
        // Whole and at least 1 is the meta-schema's rule (`number(min = 1,
        // multipleOf = 1)`, RFC 0018 exact): `1.5` is ITS finding at the
        // value, never a sentence of the loader's; `16.0` and `16` are the
        // same whole 16.
        let fraction = with(
            "            allowRefs:\n                - \"vendor/**\"\n            maxStackDepth = 1.5\n",
        );
        let err = SchemaPackage::from_parts(&fraction, resolve).unwrap_err();
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, Some(codes::FACET_VIOLATION), "{errors:?}");
        assert_eq!(
            errors[0].message,
            "'maxStackDepth' is 1.5, not a multiple of the schema's multipleOf = 1 (checked \
             exactly -- no float rounding)"
        );
        let span = errors[0].span.expect("at the value");
        assert_eq!(&fraction[span.start..span.end], "1.5");
        assert!(at.is_some(), "located: {err}");
        for whole in ["16", "16.0"] {
            let text = with(&format!(
                "            allowRefs:\n                - \"vendor/**\"\n            maxStackDepth = {whole}\n"
            ));
            let package = SchemaPackage::from_parts(&text, resolve).unwrap();
            let grant = package.manifest.validators[0]
                .layers
                .as_ref()
                .expect("granted");
            assert_eq!(grant.max_stack_depth, Some(16), "{whole}");
        }
        refused(
            &with(
                "            allowRefs:\n            denyRefs:\n                - \"vendor/vetoed/**\"\n",
            ),
            "layers.denyRefs has nothing to veto: allowRefs is empty",
            "denyRefs",
        );
        for shape in [
            "            allowRefs:\n                - \"vendor/**\"\n            maxStackDepth = 0\n",
            "            denyRefs:\n                - \"vendor/vetoed/**\"\n",
            "            allowRefs:\n                - \"vendor/**\"\n            allowComposition = true\n",
        ] {
            let err = SchemaPackage::from_parts(&with(shape), resolve).unwrap_err();
            let PackageError::Manifest { errors, .. } = &err else {
                panic!("{err:?}");
            };
            assert!(
                errors
                    .iter()
                    .all(|d| d.code != Some(codes::LAYER_GRANT_RULE)),
                "the shape is the meta-schema's: {errors:?}"
            );
        }
    }

    /// The language's inline array is a manifest list like the block
    /// form, for EVERY list-valued field — the one accessor the meta-schema
    /// admits both spellings for — and a loader rule broken by an inline
    /// element is located at that element. The loader read the block form
    /// only: an inline `files` loaded EMPTY (a false sentence), inline
    /// `allowRefs` never read, inline `budgetUnits` silently inferred.
    #[test]
    fn inline_arrays_load_like_block_lists_in_every_field() {
        let inline = MANIFEST
            .replace(
                "    rootMarkers:\n        - \"nudge.nml\"\n",
                "    rootMarkers = [\"nudge.nml\"]\n    budgetUnits = [\"tenants/*\"]\n",
            )
            .replace(
                "    modifiers:\n        - \"allow\"\n        - \"deny\"\n",
                "    modifiers = [\"allow\", deny]\n",
            )
            .replace(
                "        memberKeywords:\n            - \"role\"\n            - \"plan\"\n",
                "        memberKeywords = [\"role\", \"plan\"]\n",
            )
            .replace(
                "        builtinRefs:\n            - \"@public\"\n",
                "        builtinRefs = [\"@public\"]\n",
            )
            .replace(
                "        files:\n            - \"nudge.nml\"\n            - \"nudge.server.nml\"\n",
                "        files = [\"nudge.nml\", \"nudge.server.nml\"]\n",
            )
            .replace(
                "        schemas:\n            - server\n            - denial\n",
                "        schemas = [server, denial]\n        layers:\n            allowRefs = \
                 [\"nudge.nml\"]\n            denyRefs = [\"nudge.server.nml\"]\n",
            );
        assert_eq!(
            inline.matches(" = [").count(),
            9,
            "every list respelled inline: {inline}"
        );
        let block = MANIFEST.replace(
            "    rootMarkers:\n",
            "    budgetUnits:\n        - \"tenants/*\"\n    rootMarkers:\n",
        );
        let block = parse_manifest(&block).unwrap();
        let spelled = parse_manifest(&inline).unwrap();
        assert_eq!(spelled.root_markers, block.root_markers);
        assert_eq!(spelled.budget_units, block.budget_units);
        assert_eq!(spelled.modifiers, block.modifiers);
        assert_eq!(
            spelled.membership.member_keywords,
            block.membership.member_keywords
        );
        assert_eq!(
            spelled.membership.builtin_refs,
            block.membership.builtin_refs
        );
        assert_eq!(spelled.validators[0].files, block.validators[0].files);
        assert_eq!(spelled.validators[0].schemas, block.validators[0].schemas);
        assert_eq!(
            spelled.validators[0].file_spans.len(),
            2,
            "each inline element keeps its own span"
        );
        let second = spelled.validators[0].file_spans[1];
        assert_eq!(&inline[second.start..second.end], "\"nudge.server.nml\"");
        let grant = spelled.validators[0]
            .layers
            .as_ref()
            .expect("granted inline");
        assert_eq!(grant.allow_refs, ["nudge.nml"]);
        assert_eq!(grant.deny_refs, ["nudge.server.nml"]);
        // A rule broken by an inline element: located at the element.
        let broken = inline.replace("allowRefs = [\"nudge.nml\"]", "allowRefs = [\"nudge/**x\"]");
        assert_ne!(broken, inline);
        let err = SchemaPackage::from_parts(&broken, resolve).unwrap_err();
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(errors[0].code, Some(codes::LAYER_GRANT_RULE), "{errors:?}");
        let span = errors[0].span.expect("at the element");
        assert_eq!(&broken[span.start..span.end], "\"nudge/**x\"");
        // A veto beside an empty inline allowlist: at the `denyRefs` key.
        let veto = inline.replace("allowRefs = [\"nudge.nml\"]", "allowRefs = []");
        assert_ne!(veto, inline);
        let err = SchemaPackage::from_parts(&veto, resolve).unwrap_err();
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        let span = errors[0].span.expect("at the key");
        assert_eq!(&veto[span.start..span.end], "denyRefs", "{errors:?}");
    }

    /// RFC 0026 B-3: under inference, a glob whose inferred unit nests
    /// inside another glob's the multiplying way is a gap by construction
    /// — ONE NML2092 at the inner glob, in its NESTED form: both units and
    /// both globs named, the one declaration the loader accepts offered
    /// and the inferred boundary withdrawn (it cannot be declared beside
    /// the outer unit). A pinned nesting (a root catch-all's `*` beside
    /// `tenants/*`) is no gap; a declaration replaces the inference.
    #[test]
    fn a_nested_gap_takes_the_nested_form_at_the_inner_glob() {
        let with = |globs: &[&str], units: &str| {
            let files = globs
                .iter()
                .map(|g| format!("            - {g:?}\n"))
                .collect::<String>();
            let text = MANIFEST
                .replace(
                    "            - \"nudge.nml\"\n            - \"nudge.server.nml\"\n",
                    &files,
                )
                .replace(
                    "    formatVersion = 1\n",
                    &format!("    formatVersion = 1\n{units}"),
                );
            SchemaPackage::from_parts(&text, resolve).unwrap()
        };
        let p = with(
            &[
                "tenants/**/*.flow.nml",
                "tenants/*/plugins/*/**/*.model.nml",
            ],
            "",
        );
        let notes = crate::workspace::budget_unit_gaps(&p.manifest);
        assert_eq!(notes.len(), 1, "{notes:?}");
        let note = &notes[0];
        assert_eq!(note.code, Some(codes::BUDGET_UNIT_GAP));
        assert_eq!(note.severity, nml_core::diagnostic::Severity::Warning);
        assert!(
            note.message
                .contains("files[1] = \"tenants/*/plugins/*/**/*.model.nml\"")
                && note
                    .message
                    .contains("the inferred budget unit is `tenants/*/plugins/*`")
                && note.message.contains("nests inside `tenants/*`")
                && note
                    .message
                    .contains("files[0] = \"tenants/**/*.flow.nml\"")
                && note
                    .message
                    .contains("declare budgetUnits = [\"tenants/*\"]")
                && !note.message.contains("or ["),
            "{}",
            note.message
        );
        assert_eq!(
            &p.manifest_text[note.span.expect("at the inner glob").start..note.span.unwrap().end],
            "\"tenants/*/plugins/*/**/*.model.nml\""
        );
        // Pinned (E38's designed nesting): no gap. Declared: silent.
        assert!(
            crate::workspace::budget_unit_gaps(
                &with(&["**/*.model.nml", "tenants/**/*.flow.nml"], "").manifest,
            )
            .is_empty()
        );
        let declared = with(
            &[
                "tenants/**/*.flow.nml",
                "tenants/*/plugins/*/**/*.model.nml",
            ],
            "    budgetUnits:\n        - \"tenants/*\"\n",
        );
        assert!(crate::workspace::budget_unit_gaps(&declared.manifest).is_empty());
    }

    /// NML2093 through the meta-schema: a manifest naming `files` twice —
    /// the block spelling beside the inline array — is refused at the
    /// LATER entry, the first a related note, before the loader reads a
    /// single glob (the second `files` used to claim nothing, silently).
    #[test]
    fn manifest_with_a_repeated_entry_is_refused_at_the_later_one() {
        let dup = MANIFEST.replace(
            "        strict = true\n",
            "        strict = true\n        files = [\"nudge.nml\"]\n",
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("meta-validation refuses");
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        let first = &errors[0];
        assert_eq!(first.code, Some(codes::DUPLICATE_ENTRY), "{errors:?}");
        assert_eq!(
            first.message,
            "duplicate entry 'files' — a body declares each name once (`files:` and `files = …` \
             are two spellings of one entry)"
        );
        let inline = dup.find("files = [").expect("the inline spelling");
        assert_eq!(first.span, Some(Span::new(inline, inline + 5)));
        let block = dup.find("files:").expect("the block spelling");
        assert_eq!(first.related[0].span, Span::new(block, block + 5));
        let at = at.as_ref().expect("located");
        let loc = nml_core::span::SourceMap::new(&dup).location(inline);
        assert_eq!((at.line, at.column), (loc.line, loc.column));
        assert!(
            err.to_string().starts_with(&format!(
                "manifest failed validation at {}:{}: duplicate entry 'files'",
                loc.line, loc.column
            )),
            "{err}"
        );
    }

    /// The scalar twin: `version` twice in the package block.
    #[test]
    fn manifest_with_version_twice_is_refused() {
        let dup = MANIFEST.replace(
            "    version = \"0.1.0\"\n",
            "    version = \"0.1.0\"\n    version = \"0.2.0\"\n",
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(errors[0].code, Some(codes::DUPLICATE_ENTRY), "{errors:?}");
        assert_eq!(
            errors[0].span.map(|s| s.start),
            dup.match_indices("version = ").nth(1).map(|(i, _)| i)
        );
    }

    /// A manifest's named items are identities: two `[]validator`
    /// bindings with one name — silently loaded before, the first glob
    /// match winning — are refused at the later item, the first noted.
    #[test]
    fn two_validators_with_one_name_are_refused_at_the_later_item() {
        let dup = MANIFEST.replace(
            "        strict = true\n",
            "        strict = true\n    - server:\n        files:\n            - \"other.nml\"\n        \
             schemas:\n            - server\n",
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        let finding = &errors[0];
        assert_eq!(finding.code, Some(codes::DUPLICATE_ENTRY), "{errors:?}");
        assert_eq!(
            finding.message,
            "duplicate validator 'server' — a manifest names each validator once"
        );
        let later = dup.rfind("- server:").unwrap() + 2;
        assert_eq!(finding.span, Some(Span::new(later, later + 6)));
        let first = dup.find("[]validator validators:\n    - server:").unwrap()
            + "[]validator validators:\n    - ".len();
        assert_eq!(finding.related[0].span, Span::new(first, first + 6));
        assert_eq!(finding.related[0].message, "'server' first declared here");
    }

    /// A `[]directive` entry is its name too (`Vocabulary::get` answers the
    /// first): two entries with one name loaded silently, the first
    /// answering every lookup; refused at the later item, the first noted —
    /// the rule schemas and bindings already had.
    #[test]
    fn two_directives_with_one_name_are_refused_at_the_later_item() {
        let dup = MANIFEST.replace(
            "        doc = \"Hot-reloadable.\"\n",
            "        doc = \"Hot-reloadable.\"\n    - live:\n        arg = \"ident\"\n        doc = \"Again.\"\n",
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        let finding = &errors[0];
        assert_eq!(finding.code, Some(codes::DUPLICATE_ENTRY), "{errors:?}");
        assert_eq!(
            finding.message,
            "duplicate directive 'live' — a manifest names each directive once"
        );
        let later = dup.rfind("- live:").unwrap() + 2;
        assert_eq!(finding.span, Some(Span::new(later, later + 4)));
        let first = dup.find("- live:").unwrap() + 2;
        assert_eq!(finding.related[0].span, Span::new(first, first + 4));
        assert_eq!(finding.related[0].message, "'live' first declared here");
    }

    /// The one schema-source admission is the LOADER's too (RFC 0026
    /// decision 1): a declared `file` spelled outside `*.model.nml` /
    /// `*.schema.nml` is refused at load, at the value — it used to load,
    /// so `nml check` judged the file's directives while the editor, gated
    /// on the spelling, opened no schema pass on the same buffer. Both
    /// admitted spellings still load; the double suffix is a stem, as the
    /// kernel's own rule reads it.
    #[test]
    fn a_declared_schema_source_spelled_outside_the_admission_is_refused_at_load() {
        for (spelling, admitted) in [
            ("server.model.nml", true),
            ("server.schema.nml", true),
            ("server.model.nml.model.nml", true),
            ("server.nml", false),
            ("server.txt", false),
            ("server.MODEL.NML", false),
            ("server.model.nml.bak", false),
        ] {
            let text = MANIFEST.replace(
                "file = \"server.model.nml\"",
                &format!("file = {spelling:?}"),
            );
            assert!(
                text != MANIFEST || spelling == "server.model.nml",
                "{spelling}: the case must edit the manifest"
            );
            let resolve_any = |_: &str| Ok(SERVER_SCHEMA.to_string());
            let loaded = SchemaPackage::from_parts(&text, resolve_any);
            if admitted {
                assert!(loaded.is_ok(), "{spelling}: {:?}", loaded.err());
                continue;
            }
            let Some(err) = loaded.err() else {
                panic!("{spelling}: loads");
            };
            let PackageError::Manifest { errors, .. } = &err else {
                panic!("{spelling}: {err:?}");
            };
            let finding = &errors[0];
            assert_eq!(
                finding.code,
                Some(codes::INVALID_SCHEMA_SOURCE_NAME),
                "{spelling}: {errors:?}"
            );
            assert!(
                finding
                    .message
                    .contains("is not spelled as a schema source")
                    && finding.message.contains(".model.nml, .schema.nml")
                    && finding.message.contains(spelling),
                "{spelling}: {}",
                finding.message
            );
            // Located at the VALUE, never at the entry name.
            let span = finding.span.expect("located at the `file` value");
            assert_eq!(
                &text[span.start..span.end],
                format!("{spelling:?}"),
                "{spelling}"
            );
        }
    }

    /// A template string is a `string` to the meta-schema and nothing to
    /// the list reader: it loaded as an ABSENT element — a `denyRefs` veto
    /// that never fired, a `files` glob that claimed less than it said, a
    /// dropped unit. Every list-valued manifest entry refuses one at the
    /// element, through the one gate, in both spellings (a validator's
    /// `schemas` list is `[]schema` to the meta-schema, which refuses a
    /// template there first as an instance missing `file`).
    #[test]
    fn a_template_string_element_in_any_manifest_list_is_refused_at_the_element() {
        let block = |lines: &str| {
            MANIFEST.replace(
                "        strict = true\n",
                &format!("        strict = true\n{lines}"),
            )
        };
        let cases: Vec<(&str, String)> = vec![
            ("rootMarkers", MANIFEST.replace("    rootMarkers:\n        - \"nudge.nml\"\n", "    rootMarkers:\n        - \"nudge.nml\"\n        - \"{{m}}.nml\"\n")),
            ("budgetUnits", MANIFEST.replace("    modifiers:\n", "    budgetUnits:\n        - \"tenants/{{x}}\"\n    modifiers:\n")),
            ("modifiers", MANIFEST.replace("        - \"deny\"\n", "        - \"deny\"\n        - \"{{m}}\"\n")),
            ("memberKeywords", MANIFEST.replace("            - \"plan\"\n", "            - \"plan\"\n            - \"{{k}}\"\n")),
            ("builtinRefs", MANIFEST.replace("            - \"@public\"\n", "            - \"@public\"\n            - \"@{{r}}\"\n")),
            ("files", MANIFEST.replace("            - \"nudge.server.nml\"\n", "            - \"nudge.server.nml\"\n            - \"tenants/{{x}}/*.nml\"\n")),
            ("allowRefs", block("        layers:\n            allowRefs:\n                - \"**\"\n                - \"tenants/{{x}}\"\n")),
            ("denyRefs", block("        layers:\n            allowRefs:\n                - \"**\"\n            denyRefs:\n                - \"tenants/{{x}}\"\n")),
            // The INLINE spelling: in the block form a `schemas` item is a
            // schema object to the meta-schema, which refuses a bare string
            // before the loader reads the list; the inline array is the
            // spelling that reaches the loader's own gate.
            ("schemas", MANIFEST.replace("        schemas:\n            - server\n            - denial\n", "        schemas = [\"server\", \"{{s}}\"]\n")),
            ("files", MANIFEST.replace("        files:\n            - \"nudge.nml\"\n            - \"nudge.server.nml\"\n", "        files = [\"nudge.nml\", \"tenants/{{x}}/*.nml\"]\n")),
        ];
        for (key, text) in cases {
            assert!(text != MANIFEST, "{key}: the case must edit the manifest");
            let err = SchemaPackage::from_parts(&text, resolve)
                .expect_err(&format!("{key}: a template element loads"));
            let PackageError::Manifest { errors, .. } = &err else {
                panic!("{key}: {err:?}");
            };
            let finding = &errors[0];
            assert_eq!(
                finding.code,
                Some(codes::TEMPLATE_IN_LIST),
                "{key}: {errors:?}"
            );
            assert!(
                finding
                    .message
                    .starts_with(&format!("`{key}` holds a template string")),
                "{key}: {}",
                finding.message
            );
            let span = finding.span.expect("located at the element");
            assert!(
                text[span.start..span.end].contains("{{"),
                "{key}: {:?}",
                &text[span.start..span.end]
            );
        }
    }

    /// A second `[]validator` array, or a second `package` block, used to
    /// REPLACE the first silently: refused at the later keyword, the
    /// first noted — a loader rule, so the row is the universe's NML2088
    /// and NML2094 its cause.
    #[test]
    fn a_second_array_or_package_block_is_refused_at_its_keyword() {
        let dup = format!(
            "{MANIFEST}\n[]validator more:\n    - other:\n        files:\n            - \"other.nml\"\n        \
             schemas:\n            - server\n"
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        let finding = &errors[0];
        assert_eq!(
            finding.code,
            Some(codes::REPEATED_DECLARATION),
            "{errors:?}"
        );
        assert_eq!(
            finding.message,
            "`[]validator` is declared twice — a manifest declares `package` and each of \
             `[]schema`, `[]validator` and `[]directive` once"
        );
        let later = dup.rfind("[]validator more").unwrap() + 2;
        assert_eq!(finding.span, Some(Span::new(later, later + 9)));
        let first = dup.find("[]validator validators").unwrap() + 2;
        assert_eq!(finding.related[0].span, Span::new(first, first + 9));
        assert!(at.is_some(), "located: {err}");
        let dup =
            format!("{MANIFEST}\npackage other:\n    version = \"1\"\n    formatVersion = 1\n");
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        assert!(
            errors[0].message.starts_with("`package` is declared twice"),
            "{errors:?}"
        );
        assert_eq!(
            errors[0].span.map(|s| s.start),
            Some(dup.rfind("package other").unwrap())
        );
    }

    /// A second declaration under the SAME name — `package nudge:` twice,
    /// `[]validator validators:` twice — is the file-scope rule (NML1000),
    /// refused where the text is parsed: located at the later name, the
    /// first the note, before any slot is read.
    #[test]
    fn a_second_declaration_under_one_name_does_not_parse() {
        let name = MANIFEST
            .lines()
            .find(|l| l.starts_with("package "))
            .and_then(|l| l.strip_prefix("package "))
            .and_then(|l| l.strip_suffix(':'))
            .expect("the fixture opens with its package block");
        let dup =
            format!("{MANIFEST}\npackage {name}:\n    version = \"1\"\n    formatVersion = 1\n");
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(
            errors[0].code,
            Some(codes::DUPLICATE_DECLARATION),
            "{errors:?}"
        );
        let later = dup.rfind(&format!("package {name}")).unwrap() + "package ".len();
        assert_eq!(errors[0].span, Some(Span::new(later, later + name.len())));
        assert_eq!(
            errors[0].related[0].span.start,
            dup.find(&format!("package {name}")).unwrap() + "package ".len()
        );
        assert!(at.is_some(), "located: {err}");
        let dup = format!(
            "{MANIFEST}\n[]validator validators:\n    - other:\n        files:\n            - \"other.nml\"\n        \
             schemas:\n            - server\n"
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(
            errors[0].code,
            Some(codes::DUPLICATE_DECLARATION),
            "{errors:?}"
        );
        assert_eq!(
            errors[0].span.map(|s| s.start),
            Some(dup.rfind("validators:").unwrap())
        );
    }

    /// The same for `[]schema` sources — located now (the refusal used
    /// to carry no span at all).
    #[test]
    fn two_schemas_with_one_name_are_refused_at_the_later_item() {
        let dup = MANIFEST.replace(
            "    - denial:\n        file = \"denial.model.nml\"\n",
            "    - denial:\n        file = \"denial.model.nml\"\n    - server:\n        file = \"denial.model.nml\"\n",
        );
        let err = SchemaPackage::from_parts(&dup, resolve).expect_err("refused");
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        let finding = &errors[0];
        assert_eq!(finding.code, Some(codes::DUPLICATE_ENTRY), "{errors:?}");
        assert_eq!(
            finding.message,
            "duplicate schema 'server' — a manifest names each schema once"
        );
        let later = dup.find("    - server:\n        file = \"denial").unwrap() + 6;
        assert_eq!(finding.span, Some(Span::new(later, later + 6)));
        assert_eq!(finding.related.len(), 1);
        assert!(at.is_some(), "located: {err}");
    }

    /// RFC 0026 decision 2: every rule the loader states as a finding
    /// carries a code of its own — one concept, one code — so the
    /// manifest's NML2088 row carries it as its cause and a consumer acts
    /// on `cause.code`. Where the rule is a backstop for a fact the
    /// meta-schema states first (a required entry, an enum), the code is
    /// the meta-schema's (NML2007, NML2000): one concept, one code. The
    /// table drives every rule through the loader's front door and pins
    /// the code, the sentence's head and the location.
    #[test]
    fn every_loader_rule_is_a_coded_finding() {
        const BASE: &str = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n\
                            []schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n\
                            []validator validators:\n    - flows:\n        files:\n            \
                            - \"tenants/**/*.flow.nml\"\n        schemas:\n            - core\n";
        let with = |from: &str, to: &str| {
            let text = BASE.replace(from, to);
            assert_ne!(text, BASE, "anchor {from:?}");
            text
        };
        let cases: Vec<(&str, String, Code, &str, bool)> = vec![
            (
                "repeated",
                format!(
                    "{BASE}\n[]validator more:\n    - other:\n        files:\n            \
                     - \"x.nml\"\n        schemas:\n            - core\n"
                ),
                codes::REPEATED_DECLARATION,
                "`[]validator` is declared twice",
                true,
            ),
            (
                "no package block",
                BASE.split_once("\n\n").expect("two blocks").1.to_string(),
                codes::MISSING_DECLARATION,
                "manifest has no `package <name>:` block",
                false,
            ),
            (
                "no []schema",
                BASE.split_once("\n\n").expect("two blocks").0.to_string(),
                codes::MISSING_DECLARATION,
                "manifest declares no `[]schema` sources",
                true,
            ),
            (
                "name charset",
                with("package demo:", "package Demo:"),
                codes::INVALID_PACKAGE_NAME,
                "package name 'Demo' is not a lowercase identifier",
                true,
            ),
            (
                "unnamed entry",
                with("    - core:\n", "    - \"core\":\n"),
                codes::UNNAMED_ENTRY,
                "`[]schema` entries must be named items",
                true,
            ),
            (
                "unnamed validator entry",
                with("    - flows:\n", "    - \"flows\":\n"),
                codes::UNNAMED_ENTRY,
                "`[]validator` entries must be named items",
                true,
            ),
            (
                "unnamed directive entry",
                format!(
                    "{BASE}\n[]directive directives:\n    - \"live\":\n        arg = \"none\"\n        doc = \"x\"\n"
                ),
                codes::UNNAMED_ENTRY,
                "`[]directive` entries must be named items",
                true,
            ),
            (
                "empty binding",
                with(
                    "        schemas:\n            - core\n",
                    "        schemas = []\n",
                ),
                codes::EMPTY_BINDING,
                "`[]validator` entry 'flows' needs non-empty `files` and `schemas`",
                true,
            ),
            (
                "empty binding (the files half)",
                with(
                    "        files:\n            - \"tenants/**/*.flow.nml\"\n",
                    "        files = []\n",
                ),
                codes::EMPTY_BINDING,
                "`[]validator` entry 'flows' needs non-empty `files` and `schemas`",
                true,
            ),
            (
                "undeclared schema",
                with("            - core\n", "            - corr\n"),
                codes::UNDECLARED_SCHEMA,
                "validator 'flows' names schema 'corr', which no `[]schema` entry declares",
                true,
            ),
            (
                "binding glob",
                with("tenants/**/*.flow.nml", "tenants/**x/*.flow.nml"),
                codes::INVALID_BINDING_GLOB,
                "validator 'flows' glob 'tenants/**x/*.flow.nml': `**` must be a whole segment",
                true,
            ),
            (
                "budget unit pattern",
                with(
                    "    formatVersion = 1\n",
                    "    formatVersion = 1\n    budgetUnits = [\"tenants/**\"]\n",
                ),
                codes::BUDGET_UNIT_RULE,
                "budgetUnits entry \"tenants/**\": segment \"**\"",
                true,
            ),
            (
                "budget unit nesting",
                with(
                    "    formatVersion = 1\n",
                    "    formatVersion = 1\n    budgetUnits = [\"tenants/*\", \"tenants/*/*\"]\n",
                ),
                codes::BUDGET_UNIT_RULE,
                "budgetUnits [\"tenants/*\", \"tenants/*/*\"]: unit \"tenants/*/*\" nests inside",
                true,
            ),
            (
                "template element",
                with("tenants/**/*.flow.nml", "tenants/{{x}}/*.flow.nml"),
                codes::TEMPLATE_IN_LIST,
                "`files` holds a template string",
                true,
            ),
        ];
        for (what, text, code, head, located) in cases {
            let err = parse_manifest(&text).expect_err(what);
            let PackageError::Manifest { errors, at } = &err else {
                panic!("{what}: {err:?}");
            };
            assert_eq!(errors[0].code, Some(code), "{what}: {errors:?}");
            assert!(
                errors[0].message.starts_with(head),
                "{what}: {}",
                errors[0].message
            );
            assert_eq!(errors[0].span.is_some(), located, "{what}: {errors:?}");
            assert_eq!(at.is_some(), located, "{what}: {err}");
        }
        // The formatVersion gate keeps its values (consumers match them) and
        // contributes its coded finding for a row that wraps it.
        let err =
            parse_manifest(&with("formatVersion = 1", "formatVersion = 99")).expect_err("gate");
        assert!(
            matches!(
                err,
                PackageError::UnsupportedFormatVersion {
                    required: 99,
                    supported: SUPPORTED_FORMAT_VERSION
                }
            ),
            "{err:?}"
        );
        let gate = err.gate_finding().expect("the gate's finding");
        assert_eq!(gate.code, Some(codes::UNSUPPORTED_FORMAT_VERSION));
        assert_eq!(gate.message, err.to_string());
        assert_eq!(gate.span, None, "the gate has no place");
        assert!(parse_manifest(BASE).is_ok(), "the base manifest loads");
        // The backstops behind the meta-schema (unreachable through
        // `parse_manifest`, which meta-validates first) carry the
        // meta-schema's codes: one concept, one code.
        let file = |text: &str| nml_core::cst::parse_to_ast(text).expect("parses");
        let backstops: [(&str, String, Code, &str); 2] = [
            (
                "missing version",
                with("    version = \"0.1.0\"\n", ""),
                codes::MISSING_REQUIRED_FIELD,
                "package block is missing `version`",
            ),
            (
                "arg kind",
                format!(
                    "{BASE}\n[]directive directives:\n    - live:\n        arg = \"bogus\"\n        doc = \"x\"\n"
                ),
                codes::INVALID_ENUM_VALUE,
                "directive 'live' has unknown arg kind 'bogus'",
            ),
        ];
        for (what, text, code, head) in backstops {
            let finding = extract_manifest(&file(&text)).expect_err(what);
            assert_eq!(finding.code, Some(code), "{what}: {finding:?}");
            assert!(
                finding.message.starts_with(head),
                "{what}: {}",
                finding.message
            );
        }
    }

    /// r103-cov: the declared-file-name rule, pinned. It is the guard on
    /// EVERY resolution path — the workspace loader, `from_dir`, the
    /// discovery walk and the store's WRITE side ("a manifest must never
    /// be able to write outside its own slot", `store.rs`) — and no test
    /// named it: dropping its `..` clause left every test green. Today
    /// the `[]schema` suffix rule (NML2105) refuses `..` first for a
    /// declared source, so the clause is defence in depth; the store's
    /// writer has no second rule behind it.
    #[test]
    fn a_declared_file_name_is_a_plain_name_on_every_path() {
        for name in ["core.model.nml", "a.schema.nml", "x", "a.b.c", "-lead"] {
            assert!(check_plain_file_name(name).is_ok(), "{name:?}");
        }
        for name in [
            "",
            "..",
            "../core.model.nml",
            "a/../b.model.nml",
            "a..b.model.nml",
            "./core.model.nml",
            "sub/core.model.nml",
            "/abs.model.nml",
            "a\\b.model.nml",
        ] {
            assert_eq!(
                check_plain_file_name(name).unwrap_err(),
                "declared file names must be plain file names",
                "{name:?}"
            );
        }
    }

    /// A scratch package directory with a valid manifest + source.
    #[cfg(test)]
    fn scratch_package(tag: &str) -> std::path::PathBuf {
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nml-from-dir-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("nudge.package.nml"), MANIFEST).expect("write");
        for (file, model) in [
            ("server.model.nml", "server"),
            ("denial.model.nml", "denial"),
        ] {
            std::fs::write(
                dir.join(file),
                format!("model {model}:\n    name string+\n"),
            )
            .expect("write");
        }
        dir
    }

    /// A package DIRECTORY is read under the same two bounds the
    /// workspace walk reads a live package under — exactly. A manifest or
    /// a declared source one byte past its bound refuses the load with
    /// the kernel's ONE sentence; at the bound it loads. Before this the
    /// store's loader read both with an unbounded by-path
    /// `read_to_string`: a 2 GB `core.model.nml` in a slot was held whole,
    /// in the editor.
    #[test]
    fn a_package_directory_is_read_under_the_kernel_bounds() {
        for (over, what, bound) in [
            (false, "a package manifest", MAX_MANIFEST_BYTES),
            (true, "a package manifest", MAX_MANIFEST_BYTES),
            (false, "a declared schema source", MAX_SOURCE_BYTES),
            (true, "a declared schema source", MAX_SOURCE_BYTES),
        ] {
            let dir = scratch_package(if over { "over" } else { "at" });
            let file = if what == "a package manifest" {
                dir.join("nudge.package.nml")
            } else {
                dir.join("server.model.nml")
            };
            let head = std::fs::read_to_string(&file).unwrap();
            let pad = bound + usize::from(over) - head.len();
            // Trailing blank lines: valid NML either way, so the two
            // cases differ in ONE byte.
            std::fs::write(&file, format!("{head}{}", "\n".repeat(pad))).unwrap();
            let got = SchemaPackage::from_dir(&dir);
            if over {
                let err = got.expect_err("past the bound").to_string();
                assert!(
                    err.contains(&format!("{what} is read only up to")),
                    "{what}: {err}"
                );
            } else {
                assert!(got.is_ok(), "{what} at the bound: {:?}", got.err());
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// A declared source that is a FIFO, a device or a link is refused,
    /// never opened by path: the store's loader used to `read_to_string`
    /// it and block the editor forever.
    #[cfg(unix)]
    #[test]
    fn a_declared_source_that_is_not_a_regular_file_is_refused() {
        let dir = scratch_package("fifo");
        let source = dir.join("server.model.nml");
        std::fs::remove_file(&source).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", &source).unwrap();
        let err = SchemaPackage::from_dir(&dir)
            .expect_err("a link")
            .to_string();
        assert!(err.contains("server.model.nml"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A package directory holds exactly one manifest (NML2103) — the
    /// store slot's rule, reachable only through `from_dir`.
    #[test]
    fn a_package_directory_with_two_manifests_is_nml2103() {
        let dir = std::env::temp_dir().join(format!("nml-two-manifests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["a.package.nml", "b.package.nml"] {
            std::fs::write(dir.join(name), MANIFEST).expect("write");
        }
        let err = SchemaPackage::from_dir(&dir).expect_err("two manifests");
        let _ = std::fs::remove_dir_all(&dir);
        let PackageError::Manifest { errors, at } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(
            errors[0].code,
            Some(codes::MULTIPLE_MANIFESTS),
            "{errors:?}"
        );
        assert_eq!(
            errors[0].message,
            "package directory holds 2 manifests; exactly one <name>.package.nml is allowed"
        );
        assert!(
            at.is_none(),
            "a directory has no place in a manifest: {err}"
        );
    }
}
