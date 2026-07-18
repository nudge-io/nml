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

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use nml_core::ast::{ArrayDecl, Body, BodyEntryKind, DeclarationKind, File, ListItemKind};
use nml_core::span::Span;
use nml_core::types::{SpannedValue, Value};

use crate::diagnostics::Diagnostic;
use crate::loader::load_schema;
use crate::schema::{MembershipSemantics, SchemaValidator};

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
    /// The binding's name span in the manifest text — diagnostics about a
    /// binding (shadow warnings) point at the binding, not at (0,0).
    pub span: Span,
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
    pub modifiers: Vec<String>,
    pub membership: MembershipSemantics,
    pub schemas: Vec<SchemaEntry>,
    pub validators: Vec<ValidatorBinding>,
    pub directives: Vec<DirectiveDecl>,
}

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
    pub fn shadow_warnings(&self) -> Vec<Diagnostic> {
        let mut warnings = Vec::new();
        for (i, later) in self.validators.iter().enumerate() {
            let shadowed_by = self.validators[..i].iter().find(|earlier| {
                later
                    .files
                    .iter()
                    .all(|lg| earlier.files.iter().any(|eg| crate::glob::subsumes(eg, lg)))
            });
            if let Some(earlier) = shadowed_by {
                warnings.push(
                    Diagnostic::warning(format!(
                        "validator '{}' is fully shadowed by earlier validator '{}' (first match wins) — it can never bind a file",
                        later.name, earlier.name
                    ))
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
    pub sources: Vec<(String, String)>,
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
    /// The manifest failed to parse or meta-validate.
    Manifest { errors: Vec<Diagnostic> },
    /// The manifest is structurally sound but semantically inconsistent
    /// (bad name charset, `[]validator.schemas` naming no declared schema,
    /// duplicate logical names, missing required entry fields).
    Inconsistent { message: String, span: Option<Span> },
    /// A declared source file is missing or unreadable.
    MissingSource { file: String, detail: String },
    /// A declared source failed schema-loading (parse error, duplicate
    /// definitions, cycles). Diagnostics carry per-source attribution.
    Sources { errors: Vec<Diagnostic> },
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
            Self::Manifest { errors } => match errors.first() {
                Some(first) => write!(
                    f,
                    "manifest failed validation: {} ({} error(s))",
                    first.message,
                    errors.len()
                ),
                None => write!(f, "manifest failed validation"),
            },
            Self::Inconsistent { message, .. } => write!(f, "{message}"),
            Self::MissingSource { file, detail } => {
                write!(f, "declared source '{file}' is unavailable: {detail}")
            }
            Self::Sources { errors } => match errors.first() {
                Some(first) => write!(
                    f,
                    "schema sources failed to load: {}{} ({} error(s))",
                    first
                        .source
                        .as_deref()
                        .map(|s| format!("{s}: "))
                        .unwrap_or_default(),
                    first.message,
                    errors.len()
                ),
                None => write!(f, "schema sources failed to load"),
            },
        }
    }
}

impl SchemaPackage {
    /// Load a package from a manifest text plus its source files, resolved by
    /// the caller (embedded `include_str!` bundles, the store, a workspace).
    /// `resolve` maps a declared file name to its contents.
    pub fn from_parts(
        manifest_text: &str,
        mut resolve: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, PackageError> {
        let manifest = parse_manifest(manifest_text)?;
        let mut sources = Vec::with_capacity(manifest.schemas.len());
        for entry in &manifest.schemas {
            let text = resolve(&entry.file).map_err(|detail| PackageError::MissingSource {
                file: entry.file.clone(),
                detail,
            })?;
            sources.push((entry.name.clone(), text));
        }
        Ok(Self {
            manifest,
            sources,
            manifest_text: manifest_text.to_string(),
        })
    }

    /// Load a package from a directory holding `<name>.package.nml` and its
    /// declared sources.
    pub fn from_dir(dir: &Path) -> Result<Self, PackageError> {
        let manifest_path = find_manifest(dir)?;
        let manifest_text =
            std::fs::read_to_string(&manifest_path).map_err(|e| PackageError::MissingSource {
                file: manifest_path.display().to_string(),
                detail: e.to_string(),
            })?;
        Self::from_parts(&manifest_text, |file| {
            check_plain_file_name(file)?;
            std::fs::read_to_string(dir.join(file)).map_err(|e| e.to_string())
        })
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

    /// Build the validator for one binding: the composed schema set named by
    /// `binding.schemas`, with the package's profile applied (strictness,
    /// modifiers, membership). Binding is exclusive by construction — only
    /// the package's own sources participate.
    pub fn validator(&self, binding: &ValidatorBinding) -> Result<SchemaValidator, PackageError> {
        let selected: Vec<(&str, &str)> = self
            .sources
            .iter()
            .filter(|(name, _)| binding.schemas.iter().any(|s| s == name))
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let (schema, diags) = load_schema(&selected);
        let errors: Vec<Diagnostic> = diags
            .into_iter()
            .filter(|d| matches!(d.severity, crate::diagnostics::Severity::Error))
            .collect();
        if !errors.is_empty() {
            return Err(PackageError::Sources { errors });
        }
        let mut validator = SchemaValidator::new(schema.models, schema.enums, schema.oneofs)
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
        self.manifest
            .validators
            .iter()
            .find(|b| b.files.iter().any(|g| crate::glob::glob_match(g, path)))
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

fn find_manifest(dir: &Path) -> Result<std::path::PathBuf, PackageError> {
    let entries = std::fs::read_dir(dir).map_err(|e| PackageError::MissingSource {
        file: dir.display().to_string(),
        detail: e.to_string(),
    })?;
    let mut manifests: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".package.nml") && n != "package.nml")
        })
        .collect();
    manifests.sort();
    match manifests.len() {
        0 => Err(PackageError::MissingSource {
            file: dir.display().to_string(),
            detail: "no <name>.package.nml manifest found".to_string(),
        }),
        1 => Ok(manifests.remove(0)),
        _ => Err(PackageError::Inconsistent {
            message: format!(
                "package directory holds {} manifests; exactly one <name>.package.nml is allowed",
                manifests.len()
            ),
            span: None,
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
            return Err(PackageError::Manifest {
                errors: vec![Diagnostic::error(e.message().to_string()).with_span(e.span())],
            });
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
    let validator = SchemaValidator::new(meta.models, meta.enums, meta.oneofs).strict();
    let errors: Vec<Diagnostic> = validator
        .validate(&file)
        .into_iter()
        .filter(|d| matches!(d.severity, crate::diagnostics::Severity::Error))
        .collect();
    if !errors.is_empty() {
        return Err(PackageError::Manifest { errors });
    }

    extract_manifest(&file)
}

/// Text-level formatVersion scan for manifests that fail to parse.
fn scan_format_version_text(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("formatVersion")?.trim_start();
        rest.strip_prefix('=')?.trim().parse().ok()
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

fn number_as_u64(v: &SpannedValue) -> Option<u64> {
    match &v.value {
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

fn extract_manifest(file: &File) -> Result<PackageManifest, PackageError> {
    let mut package_block = None;
    let mut schemas_decl: Option<&ArrayDecl> = None;
    let mut validators_decl: Option<&ArrayDecl> = None;
    let mut directives_decl: Option<&ArrayDecl> = None;

    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) if block.keyword.name == "package" => {
                package_block = Some(block);
            }
            DeclarationKind::Array(arr) => match arr.item_keyword.name.as_str() {
                "schema" => schemas_decl = Some(arr),
                "validator" => validators_decl = Some(arr),
                "directive" => directives_decl = Some(arr),
                _ => {}
            },
            _ => {}
        }
    }

    let block = package_block.ok_or_else(|| PackageError::Inconsistent {
        message: "manifest has no `package <name>:` block".to_string(),
        span: None,
    })?;
    let name = block.name.name.clone();
    if !valid_package_name(&name) {
        return Err(PackageError::Inconsistent {
            message: format!(
                "package name '{name}' is not a lowercase identifier ([a-z][a-z0-9-]*)"
            ),
            span: Some(block.name.span),
        });
    }

    let mut version = None;
    let mut format_version = None;
    let mut root_markers = Vec::new();
    let mut modifiers = Vec::new();
    let mut membership = MembershipSemantics::default();
    for entry in &block.body.entries {
        match &entry.kind {
            BodyEntryKind::Property(p) => match p.name.name.as_str() {
                "version" => version = string_value(&p.value),
                "formatVersion" => format_version = number_as_u64(&p.value),
                _ => {}
            },
            BodyEntryKind::NestedBlock(nb) => match nb.name.name.as_str() {
                "rootMarkers" => root_markers = string_list(&nb.body),
                "modifiers" => modifiers = string_list(&nb.body),
                "membership" => membership = extract_membership(&nb.body),
                _ => {}
            },
            _ => {}
        }
    }
    let version = version.ok_or_else(|| PackageError::Inconsistent {
        message: "package block is missing `version`".to_string(),
        span: Some(block.name.span),
    })?;
    let format_version = format_version.ok_or_else(|| PackageError::Inconsistent {
        message: "package block is missing `formatVersion`".to_string(),
        span: Some(block.name.span),
    })?;

    let schemas = extract_schemas(schemas_decl)?;
    if schemas.is_empty() {
        return Err(PackageError::Inconsistent {
            message: "manifest declares no `[]schema` sources".to_string(),
            span: Some(block.name.span),
        });
    }
    let declared: HashSet<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
    if declared.len() != schemas.len() {
        return Err(PackageError::Inconsistent {
            message: "duplicate logical schema names in `[]schema`".to_string(),
            span: None,
        });
    }

    let validators = extract_validators(validators_decl, &declared)?;
    let directives = extract_directives(directives_decl)?;

    Ok(PackageManifest {
        name,
        version,
        format_version,
        root_markers,
        modifiers,
        membership,
        schemas,
        validators,
        directives,
    })
}

fn extract_schemas(decl: Option<&ArrayDecl>) -> Result<Vec<SchemaEntry>, PackageError> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(PackageError::Inconsistent {
                message: "`[]schema` entries must be named items (`- name:`)".to_string(),
                span: Some(item.span),
            });
        };
        // Loader-level required-field backstop (RFC 0030 spike finding): a
        // `file`-less entry must fail here with a precise message, not later
        // as an opaque read error.
        let file =
            body_property_string(body, "file").ok_or_else(|| PackageError::Inconsistent {
                message: format!("`[]schema` entry '{}' is missing `file`", name.name),
                span: Some(name.span),
            })?;
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
) -> Result<Vec<ValidatorBinding>, PackageError> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(PackageError::Inconsistent {
                message: "`[]validator` entries must be named items (`- name:`)".to_string(),
                span: Some(item.span),
            });
        };
        let files = body_string_list(body, "files");
        let schemas = body_string_list(body, "schemas");
        if files.is_empty() || schemas.is_empty() {
            return Err(PackageError::Inconsistent {
                message: format!(
                    "`[]validator` entry '{}' needs non-empty `files` and `schemas`",
                    name.name
                ),
                span: Some(name.span),
            });
        }
        for glob in &files {
            // Glob meta-validation: `**` is a whole-segment wildcard only —
            // embedded in a segment it would silently degrade to `*`
            // semantics; reject it loudly instead. The segment cap mirrors
            // the matcher's DoS bound so a rejected pattern never reaches it.
            if glob.split('/').any(|seg| seg.contains("**") && seg != "**") {
                return Err(PackageError::Inconsistent {
                    message: format!(
                        "validator '{}' glob '{glob}': `**` must be a whole segment",
                        name.name
                    ),
                    span: Some(name.span),
                });
            }
            if glob.split('/').count() > crate::glob::MAX_PATTERN_SEGMENTS {
                return Err(PackageError::Inconsistent {
                    message: format!(
                        "validator '{}' glob '{glob}' exceeds {} segments",
                        name.name,
                        crate::glob::MAX_PATTERN_SEGMENTS
                    ),
                    span: Some(name.span),
                });
            }
        }
        for s in &schemas {
            if !declared_schemas.contains(s.as_str()) {
                return Err(PackageError::Inconsistent {
                    message: format!(
                        "validator '{}' names schema '{s}', which no `[]schema` entry declares",
                        name.name
                    ),
                    span: Some(name.span),
                });
            }
        }
        out.push(ValidatorBinding {
            name: name.name.clone(),
            files,
            schemas,
            strict: body_property_bool(body, "strict").unwrap_or(false),
            span: name.span,
        });
    }
    Ok(out)
}

fn extract_directives(decl: Option<&ArrayDecl>) -> Result<Vec<DirectiveDecl>, PackageError> {
    let Some(decl) = decl else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in &decl.body.items {
        let ListItemKind::Named { name, body } = &item.kind else {
            return Err(PackageError::Inconsistent {
                message: "`[]directive` entries must be named items (`- name:`)".to_string(),
                span: Some(item.span),
            });
        };
        let arg_raw =
            body_property_string(body, "arg").ok_or_else(|| PackageError::Inconsistent {
                message: format!("`[]directive` entry '{}' is missing `arg`", name.name),
                span: Some(name.span),
            })?;
        let arg = DirectiveArg::parse(&arg_raw).ok_or_else(|| PackageError::Inconsistent {
            message: format!(
                "directive '{}' has unknown arg kind '{arg_raw}' (none|ident|string|number)",
                name.name
            ),
            span: Some(name.span),
        })?;
        let doc = body_property_string(body, "doc").ok_or_else(|| PackageError::Inconsistent {
            message: format!("`[]directive` entry '{}' is missing `doc`", name.name),
            span: Some(name.span),
        })?;
        out.push(DirectiveDecl {
            name: name.name.clone(),
            arg,
            doc,
        });
    }
    Ok(out)
}

fn extract_membership(body: &Body) -> MembershipSemantics {
    let mut m = MembershipSemantics::default();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::NestedBlock(nb) => match nb.name.name.as_str() {
                "memberKeywords" => m.member_keywords = string_list(&nb.body),
                "builtinRefs" => m.builtin_refs = string_list(&nb.body),
                _ => {}
            },
            BodyEntryKind::Property(p) if p.name.name == "userRefPrefix" => {
                m.user_ref_prefix = string_value(&p.value);
            }
            _ => {}
        }
    }
    m
}

fn string_value(v: &SpannedValue) -> Option<String> {
    match &v.value {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// String-ish entries of a list body: quoted strings (`- "x"`) and bare
/// identifiers (`- server`, lowered as references).
fn string_list(body: &Body) -> Vec<String> {
    let mut out = Vec::new();
    for entry in &body.entries {
        if let BodyEntryKind::ListItem(item) = &entry.kind {
            match &item.kind {
                ListItemKind::Shorthand { value, .. } => {
                    if let Value::String(s) = &value.value {
                        out.push(s.clone());
                    }
                }
                ListItemKind::Reference(id) => out.push(id.name.clone()),
                _ => {}
            }
        }
    }
    out
}

fn body_property_string(body: &Body, key: &str) -> Option<String> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == key => string_value(&p.value),
        _ => None,
    })
}

fn body_property_bool(body: &Body, key: &str) -> Option<bool> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == key => match &p.value.value {
            Value::Bool(b) => Some(*b),
            _ => None,
        },
        _ => None,
    })
}

fn body_string_list(body: &Body, key: &str) -> Vec<String> {
    body.entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == key => Some(string_list(&nb.body)),
            _ => None,
        })
        .unwrap_or_default()
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
    }

    /// The parity-bearing path: a validator built from the package composes
    /// the named schema set, applies strictness + modifiers + membership, and
    /// produces boot-identical diagnostics (did-you-mean, unknown key,
    /// cross-schema refs resolving).
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
                .any(|d| d.message.contains("did you mean \"Lax\"") && d.suggestion.is_some()),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.message.contains("unknownKey")
                && matches!(d.severity, crate::diagnostics::Severity::Error)),
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
            Err(PackageError::Manifest { .. }) | Err(PackageError::Inconsistent { .. })
        ));
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
            Err(PackageError::Manifest { errors }) => {
                // The lowering fix makes `- server:` a Named empty item, so
                // strict meta-validation reports the missing `file` itself.
                assert!(
                    errors
                        .iter()
                        .any(|e| e.message.contains("missing required field 'file'")),
                    "{errors:?}"
                );
            }
            Err(PackageError::Inconsistent { message, .. }) => {
                assert!(message.contains("missing `file`"), "{message}");
            }
            other => panic!("expected a precise missing-file error, got {other:?}"),
        }
    }

    #[test]
    fn validator_naming_undeclared_schema_is_rejected() {
        let broken = MANIFEST.replace("            - denial\n", "            - nonexistent\n");
        match SchemaPackage::from_parts(&broken, resolve) {
            Err(PackageError::Inconsistent { message, .. }) => {
                assert!(message.contains("nonexistent"), "{message}");
            }
            other => panic!("expected Inconsistent, got {other:?}"),
        }
    }

    #[test]
    fn package_name_charset_is_enforced() {
        let broken = MANIFEST.replace("package nudge:", "package Nudge:");
        assert!(matches!(
            SchemaPackage::from_parts(&broken, resolve),
            Err(PackageError::Inconsistent { .. })
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
            .filter(|d| matches!(d.severity, crate::diagnostics::Severity::Error))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }
}
