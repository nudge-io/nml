//! The diagnostics model for NML — one type for every finding the toolchain
//! reports (RFC 0008).
//!
//! **Abort vs. report:** [`crate::error::NmlError`] is the *abort* error for
//! `Result` signatures (it implements `std::error::Error`);
//! [`Diagnostic`](crate::diagnostic::Diagnostic) is a *findings report* —
//! data, deliberately not an error trait object. Every surface that reports
//! findings (validator, symbols, parser error lists, LSP, CLI) speaks this
//! type, so hints, codes, and rendering exist exactly once.
//!
//! **Hints are derived, never hand-written:** producers attach a structured
//! [`Suggestion`](crate::diagnostic::Suggestion);
//! [`rendered_message`](crate::diagnostic::Diagnostic::rendered_message) is
//! the single renderer that turns it into the human-facing
//! `(did you mean "…"?)` text. Baking hint prose into `message` is a bug.
//!
//! **Codes are forever:** a [`Code`](crate::diagnostic::Code) is stable from
//! the first published release onward — never renumbered, never reused (see
//! `docs/stability.md`). The constants in [`codes`](crate::diagnostic::codes)
//! are the only way to mint one, so the rule is enforced by construction.
//! Their doc comments seed the error-index pages.

use std::fmt;

use crate::span::Span;

/// The severity level of a diagnostic.
///
/// `non_exhaustive`: severities may grow (e.g. a hint level) without a
/// breaking change; match with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    Error,
    Warning,
    /// Advisory notices (e.g. the RFC 0030 undeclared-sibling notice) —
    /// surfaced, but neither a failure nor a warning.
    Info,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Info => write!(f, "info"),
        }
    }
}

/// What a [`Suggestion`] *is* — the axis is exclusivity/applicability, not
/// rendering prose (though rendering derives from it):
///
/// * [`DidYouMean`](SuggestionKind::DidYouMean) — a **singular** correction
///   for a near-miss (typo'd enum value, directive, variant). Machine-
///   applicable; an editor may mark it preferred / auto-apply it when it is
///   the only suggestion.
/// * [`Fix`](SuggestionKind::Fix) — **one of N mutually exclusive
///   alternatives** (e.g. RFC 0015 D2's "annotate as `modelA`" / "…`modelB`").
///   NEVER auto-applied or preferred when siblings exist: the editor silently
///   picking one would resurrect exactly the guess the diagnostic exists to
///   forbid. A future `nml fix --apply` applies `Fix`es only when singular.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    DidYouMean,
    Fix,
}

/// A machine-applicable edit carried alongside a diagnostic (RFC 0030): the
/// exact replacement text and the exact span it replaces. Produced wherever
/// a correction is *derivable* (e.g. a did-you-mean), so editors can offer a
/// one-keystroke quick-fix instead of leaving the suggestion trapped in
/// message prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The text to insert at `span` (for a string value: the bare content,
    /// without quotes — `span` covers the string's content, not its quotes).
    pub replacement: String,
    /// The exact range the replacement substitutes.
    pub span: Span,
    /// The exclusivity semantics — see [`SuggestionKind`].
    pub kind: SuggestionKind,
}

/// A stable diagnostic code (`NML0042`).
///
/// The inner number is private: codes are constructible only from the vetted
/// constants in [`codes`], so "never renumbered, never reused" is a compile
/// guarantee, not a convention. [`fmt::Display`] is the only accessor — both
/// consumers (the CLI's `error[NML0042]` prefix and the LSP's string `code`
/// field) want the formatted form; a numeric getter would be speculative API
/// until something needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code(u16);

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NML{:04}", self.0)
    }
}

/// Declares the code constants and (in tests) the `ALL` list from one
/// source, so the uniqueness test can never drift from the declarations.
macro_rules! codes {
    ($($(#[$doc:meta])* $name:ident = $num:literal;)+) => {
        $($(#[$doc])* pub const $name: Code = Code($num);)+
        #[cfg(test)]
        pub(crate) const ALL: &[(&str, Code)] = &[$((stringify!($name), $name)),+];
    };
}

/// The stable code space, banded by subsystem for allocation convenience
/// (bands are **not** API — a diagnostic moving between subsystems keeps its
/// code): 0001–0999 lex/parse · 1000–1999 symbols & resolution · 2000–2999
/// schema loading & validation · 3000–3999 values & money · 4000–4999
/// packages & store · 5000–5999 editor/LSP. Sites not yet assigned a code
/// emit `None`; the docs plan's Phase 4 sweep completes coverage. Every code
/// has a section in the error index (`docs/errors/README.md`) — enforced
/// bidirectionally by `just docs-test`.
pub mod codes {
    use super::Code;

    codes! {
        /// Removed syntax with a mechanical replacement — see the migration
        /// ledger in the error index (the fixers commitment,
        /// `docs/stability.md`).
        REPLACED_SYNTAX = 1;
        /// The parser met a token that fits no expected alternative.
        UNEXPECTED_TOKEN = 2;
        /// A string literal is missing its closing delimiter.
        UNTERMINATED_STRING = 3;
        /// A byte no NML token starts with.
        UNEXPECTED_CHARACTER = 4;
        /// A tab character in indentation (the spec requires spaces).
        TAB_IN_INDENT = 5;
        /// A dedent to a column that matches no enclosing block (the
        /// offside rule).
        BAD_DEDENT = 6;
        /// A deliberate nesting bound was exceeded (DoS defense on
        /// untrusted input; the index documents the limits).
        NESTING_LIMIT = 7;
        /// `set<a, b>` — set elements are alternatives (`|`), not a list.
        /// Machine-fixable.
        SET_SEPARATOR = 8;
        /// `map` is reserved for a future map type.
        RESERVED_TYPE_KEYWORD = 9;
        /// An identifier takes type arguments but only `set` is a
        /// constructor; comes with a did-you-mean.
        UNKNOWN_TYPE_CONSTRUCTOR = 10;
        /// The same `#directive` key twice on one field.
        DUPLICATE_DIRECTIVE = 11;
        /// An unknown or unterminated string escape.
        INVALID_ESCAPE = 12;
        /// A numeric literal no number parses from.
        INVALID_NUMBER = 13;
        /// An integer outside `i64` (numbers are exact by design).
        NUMBER_OUT_OF_RANGE = 14;
        /// A malformed `$NS.key` variable reference.
        BAD_SECRET_REF = 15;
        /// A carriage return with no following line feed (spec: Source
        /// text — line endings are LF or CRLF).
        BARE_CARRIAGE_RETURN = 16;
        /// A raw control character (C0 minus tab/line endings, or DEL)
        /// in source; the `\u{…}` escape is the sanctioned spelling.
        FORBIDDEN_CONTROL = 17;
        /// An invisible character that can make source display
        /// differently than it parses (bidirectional controls, interior
        /// U+FEFF) — the Trojan Source defense.
        INVISIBLE_CHARACTER = 18;
        /// Content on a multi-line string's opening line (content must
        /// begin on a new line — the Swift/Java text-block rule).
        MULTILINE_OPENING_CONTENT = 19;
        /// An own-line closing `"""` not aligned with the content's
        /// indentation (machine-fixable: move the delimiter).
        MULTILINE_CLOSING_MISALIGNED = 20;

        /// The same name is declared more than once in one namespace.
        DUPLICATE_DECLARATION = 1000;
        /// A value references a name that no declaration defines.
        UNRESOLVED_REFERENCE = 1001;
        /// `const` definitions form a reference cycle.
        CONST_CYCLE = 1002;

        /// A value is not one of the enum's declared variants.
        INVALID_ENUM_VALUE = 2000;
        /// A property is not defined by the governing model.
        UNKNOWN_PROPERTY = 2001;
        /// A modifier name is not in the configured modifier set.
        UNKNOWN_MODIFIER = 2002;
        /// A `oneof` discriminator value matches no declared variant.
        UNKNOWN_DISCRIMINANT = 2003;
        /// A block keyword has no model or `oneof` definition (strict mode).
        UNKNOWN_BLOCK_KEYWORD = 2004;
        /// An array item keyword has no model or `oneof` definition (strict mode).
        UNKNOWN_ARRAY_KEYWORD = 2005;
        /// A `secret` field holds a literal instead of a reference.
        SECRET_LITERAL = 2006;
        /// A field the model requires is absent from the instance.
        MISSING_REQUIRED_FIELD = 2007;
        /// A value's type does not match the field's declared type.
        TYPE_MISMATCH = 2008;
        /// The same model/enum/oneof name is defined more than once in a
        /// schema set.
        DUPLICATE_DEFINITION = 2009;
        /// A definition uses a reserved type-constructor name (`set`, `map`).
        RESERVED_TYPE_NAME = 2010;
        /// A model declares more than one positional (`+`) field.
        MULTIPLE_POSITIONAL_FIELDS = 2011;
        /// A `oneof` arm references a model that is not declared.
        ONEOF_INTEGRITY = 2012;
        /// A `oneof` declares the same discriminator value twice.
        DUPLICATE_DISCRIMINANT = 2015;
        /// A `oneof` name collides with a model or enum name.
        ONEOF_NAME_COLLISION = 2016;
        /// A `oneof` default discriminator matches none of its arms.
        ONEOF_BAD_DEFAULT = 2017;
        /// A `oneof` discriminator type does not name a declared enum.
        ONEOF_BAD_DISCRIMINANT_TYPE = 2018;
        /// A `oneof` with an enum-typed discriminator does not cover the
        /// enum exactly (missing arm, or arm outside the enum).
        ONEOF_NOT_EXHAUSTIVE = 2019;
        /// An `is` target does not resolve to any model or trait (RFC 0011).
        UNKNOWN_MIXIN = 2020;
        /// An `is` target names an enum or `oneof` — only models and traits
        /// compose (RFC 0011).
        INVALID_MIXIN_KIND = 2021;
        /// A field's type references a trait — traits are composition-only,
        /// never value types (RFC 0011).
        TRAIT_AS_FIELD_TYPE = 2022;
        /// A `oneof` arm targets a trait — variants must be instantiable
        /// models (RFC 0011).
        TRAIT_ONEOF_VARIANT = 2023;
        /// A block or array keyword names a trait — traits cannot be
        /// instantiated (RFC 0011).
        TRAIT_INSTANTIATED = 2024;
        /// A model/trait `is` clause lists the same mixin twice — the merge
        /// is idempotent, so the duplicate is noise (composition tidiness).
        DUPLICATE_MIXIN = 2025;
        /// In-file schema definitions under a closed package binding have no
        /// effect (RFC 0012): the binding's schemas are the entire
        /// vocabulary.
        INEFFECTIVE_DEFINITIONS = 2026;
        /// An enum declares the same variant twice (both authored forms —
        /// `- "a"` and `- a` — name one variant).
        DUPLICATE_ENUM_VARIANT = 2027;
        /// An enum declares no variants — no instance value can satisfy a
        /// field it types, and an `as`-typed `oneof` can never cover it.
        EMPTY_ENUM = 2028;
        /// A `duration`-typed value does not match the duration grammar
        /// (spec: a numeric value with unit `h`/`m`/`s`/`ms`, e.g. "30s").
        INVALID_DURATION = 2029;
        /// A set contains the same element more than once (element identity
        /// is value-level; sets are unique by definition).
        DUPLICATE_SET_ELEMENT = 2030;
        /// A non-arm entry appears in a `(K -> V)`-typed field's body,
        /// which holds only routing arms.
        ARMS_BODY_ENTRY = 2031;
        /// A value matches none of a union type's variants.
        UNION_TYPE_MISMATCH = 2032;
        /// A type composition with no instance form (RFC 0007 §4.3): an arm
        /// set in a position whose body can never hold arms, or a union
        /// with more than one arm-set variant.
        INVALID_TYPE_SHAPE = 2033;
        /// A field definition outside a model/trait declaration.
        MISPLACED_FIELD_DEFINITION = 2034;
        /// Routing arm entries inside a schema declaration — arms belong in
        /// instances; the declaration carries the `(K -> V)` type.
        ARMS_IN_DEFINITION = 2035;
        /// An arm set repeats a selector: a second `else`, or a duplicate
        /// arm key — dispatch would be ambiguous (first match wins).
        DUPLICATE_ARM = 2036;
        /// An arm after `else` can never match — arms match first-to-last,
        /// so `else` must be the final arm.
        UNREACHABLE_ARM = 2037;
        /// An arm's selector does not conform to the declared key type.
        ARM_KEY_MISMATCH = 2038;
        /// A string-literal arm target where the arm set's target type is
        /// not scalar-capable — use a declared name instead.
        ARM_TARGET_MISMATCH = 2039;
        /// Routing arms in a model-typed body — arms belong under a field
        /// typed `(K -> V)`.
        ARMS_NOT_EXPECTED = 2040;
        /// A `oneof` instance omits its discriminator and the union declares
        /// no default arm.
        MISSING_DISCRIMINATOR = 2041;
        /// A `oneof` discriminator value that is not a string.
        INVALID_DISCRIMINATOR = 2042;
        /// A scalar shorthand item on a union-typed list — the variant is
        /// undecidable from a bare scalar; write the block form.
        UNION_SHORTHAND = 2043;
        /// Validation stopped descending at the maximum nesting depth;
        /// deeper entries were not checked (advisory).
        VALIDATION_TRUNCATED = 2044;
        /// A quoted string in a `role`-typed field — roles are references
        /// (`@name`), not strings; machine-fixable.
        ROLE_LITERAL = 2045;
        /// A user reference (`@user/…`) in an access-control rule — user
        /// refs belong in members lists.
        USER_REF_IN_ACL = 2046;
        /// A built-in access level (`@public`, …) in a members list.
        BUILTIN_IN_MEMBERS = 2047;
        /// Role/plan membership references form a cycle.
        MEMBERSHIP_CYCLE = 2048;
        /// A bare scalar item's key was dropped: the element model declares
        /// no positional (`+`) field to receive it.
        DROPPED_ITEM_KEY = 2049;

        /// A list item's BODY has nowhere to go: the element type is a
        /// scalar/union/collection with no fields to fill — the body-side
        /// mirror of the dropped-key rule above.
        DROPPED_ITEM_BODY = 2055;

        /// A scalar item cannot fill an arm-set shorthand field — an arm
        /// target is a name or a string, so no arm can be synthesized from
        /// this value (RFC 0005 §10).
        ARM_SHORTHAND_MISMATCH = 2050;
        /// An `as <Variant>` nominal annotation (RFC 0015) names a type that is
        /// not one of the union's variants; comes with a did-you-mean.
        UNKNOWN_UNION_VARIANT = 2051;
        /// A same-class union instance carries no `as <Variant>` annotation and
        /// its body shape cannot choose between two or more model variants
        /// (RFC 0015 D2). Fail-closed: the author must state the type.
        AMBIGUOUS_UNION_INSTANCE = 2052;
        /// An `as <Variant>` annotation (RFC 0015) sits on a field that is not a
        /// union — there is no variant to select, so the annotation has no
        /// meaning. Flagged rather than silently ignored (visible-never-silent).
        STRAY_TYPE_ANNOTATION = 2053;
        /// A oneof variant model declares a field named like the
        /// discriminator — unreachable, the property is always claimed as
        /// the discriminator (advisory).
        SHADOWED_DISCRIMINATOR = 2054;

        /// A package validator binding is fully shadowed by earlier
        /// bindings — its globs can never match first (RFC 0030).
        SHADOWED_VALIDATOR = 4000;
        /// Model `extends` chains form a cycle.
        EXTENDS_CYCLE = 2013;
        /// Model references form a cycle (advisory: legal, but often a sign
        /// of an unintended self-reference).
        MODEL_REFERENCE_CYCLE = 2014;

        /// A money literal is malformed (unparseable amount or fraction).
        INVALID_MONEY = 3000;
        /// A money literal names a currency code not in the ISO 4217 table.
        UNKNOWN_CURRENCY = 3001;
        /// More fractional digits than the currency's minor unit allows
        /// (`19.999 USD` — USD has 2).
        MONEY_PRECISION = 3002;
        /// The scaled minor-unit amount exceeds `i64` (money is exact by
        /// design, never floated).
        AMOUNT_OUT_OF_RANGE = 3003;

        /// A directive name is not in the covering package's vocabulary.
        UNKNOWN_DIRECTIVE = 5000;
        /// A directive's argument does not match its declared arity.
        DIRECTIVE_BAD_ARITY = 5001;
        /// Contradictory directives on one field (e.g. `#live` + `#restart`).
        DIRECTIVE_CONFLICT = 5002;
        /// A sibling schema file is not declared in the package manifest.
        UNDECLARED_SIBLING = 5003;
        /// A template expression uses a namespace the project does not configure.
        UNKNOWN_TEMPLATE_NAMESPACE = 5004;
    }
}

/// The error index source (`## NML0000` sections) — embedded so explanations
/// work offline in every consumer (`nml explain`, future editor hovers).
/// The docs-test guard keeps it bidirectionally complete against [`codes`].
const ERROR_INDEX: &str = include_str!("../assets/error-index.md");

/// The `(code, body)` pairs of the index's `## NML0000` sections, in index
/// order — the one place that knows the index's section shape. Every public
/// derivation ([`explain`], [`explain_summary`], [`explain_document`],
/// [`explain_index`]) rides this iterator, so the shape is parsed in exactly
/// one spot and the derivations can never disagree about it. The leading
/// element of the split is the preamble (title, stability notes) — skipped:
/// it is browsing context, never explanation content.
fn sections() -> impl Iterator<Item = (&'static str, &'static str)> {
    ERROR_INDEX.split("\n## ").skip(1).filter_map(|s| {
        let (head, body) = s.split_once('\n')?;
        Some((head.trim(), body.trim()))
    })
}

/// The error-index section for `code` (e.g. `"NML2007"`), without its
/// heading line — the offline body behind `nml explain`. `None` when the
/// code has no section (unknown or unreleased code strings).
pub fn explain(code: &str) -> Option<&'static str> {
    sections().find_map(|(head, body)| (head == code).then_some(body))
}

/// The first paragraph of a code's index section — the bounded hover summary
/// (RFC 0010 tier 1: the meaning line, never the examples; hover real estate
/// is precious). One splitter beside [`explain`], never re-derived per
/// consumer. Relative markdown links are stripped to their text (they would
/// dangle in hover context); absolute `http(s)` links are kept — they become
/// useful the day the index is published.
pub fn explain_summary(code: &str) -> Option<String> {
    summary_of(explain(code)?)
}

/// First paragraph → one line → links policy. Shared by [`explain_summary`]
/// (one code) and [`explain_index`] (all codes).
fn summary_of(body: &str) -> Option<String> {
    let para = body.split("\n\n").next()?.trim().replace('\n', " ");
    Some(strip_relative_links(&para))
}

/// The full standalone document for a code — a `# NML2007` heading plus the
/// complete index section (RFC 0010 tier 2). One composer for every full-
/// entry surface: the CLI's `nml explain` and the editor's `nml/explain`
/// virtual document render this byte-for-byte, so "the full entry" has
/// exactly one shape. The heading interpolates the **matched section head**,
/// never the caller's string — only the vetted code strings can ever appear
/// in output, by construction. The link policy matches [`explain_summary`],
/// applied line-wise *outside* code fences: a fenced example is content,
/// never rewritten.
pub fn explain_document(code: &str) -> Option<String> {
    sections().find_map(|(head, body)| (head == code).then(|| compose_document(head, body)))
}

/// `# {head}` + the body with the link policy applied outside fences — the
/// one place a full-entry document is shaped (see [`explain_document`]).
fn compose_document(head: &str, body: &str) -> String {
    let mut doc = format!("# {head}\n\n");
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        let is_fence_delimiter = line.trim_start().starts_with("```");
        if is_fence_delimiter {
            in_fence = !in_fence;
        }
        if in_fence || is_fence_delimiter {
            doc.push_str(line);
        } else {
            doc.push_str(&strip_relative_links(line));
        }
    }
    doc
}

/// Every `(code, summary)` pair in the index, in index order — the
/// discoverability surface behind the editor's `nml/explainIndex` (the
/// explain-a-code palette) and the CLI's `nml explain --list`. Derived from
/// the index itself rather than [`codes`], so it needs no runtime code
/// enumeration and can never disagree with what [`explain`] serves; the
/// docs-test guard keeps index↔codes bidirectionally complete.
pub fn explain_index() -> Vec<(&'static str, String)> {
    sections()
        .filter_map(|(head, body)| Some((head, summary_of(body)?)))
        .collect()
}

/// Rewrite `[text](target)` to `text` for non-absolute targets, keeping
/// absolute links intact. A hand-rolled scan (no regex): the index is our
/// own review-guarded content — simple links only, and an unmatched shape
/// passes through verbatim rather than being mangled.
fn strip_relative_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(mid_rel) = rest[open..].find("](") else {
            break;
        };
        let mid = open + mid_rel;
        let Some(close_rel) = rest[mid + 2..].find(')') else {
            break;
        };
        let close = mid + 2 + close_rel;
        let label = &rest[open + 1..mid];
        let target = &rest[mid + 2..close];
        out.push_str(&rest[..open]);
        if target.starts_with("http://") || target.starts_with("https://") {
            out.push_str(&rest[open..=close]);
        } else {
            out.push_str(label);
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}
/// One reported finding. Constructed via the builders ([`Diagnostic::error`]
/// et al.) — `non_exhaustive`, so fields may be added without a breaking
/// change.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Stable code, when this site has been assigned one.
    pub code: Option<Code>,
    pub severity: Severity,
    /// Prose statement of the finding. Hint text is **not** part of the
    /// message — it renders from `suggestion` via [`Self::rendered_message`].
    pub message: String,
    pub span: Option<Span>,
    /// The source document this diagnostic belongs to, for multi-source
    /// loads (RFC 0030 schema packages) — spans from different sources are
    /// numerically ambiguous without it. `None` for single-source contexts
    /// and for cross-source findings that no one file owns.
    pub source: Option<String>,
    /// Machine-applicable edits, when derivable. One `DidYouMean` for a
    /// near-miss; N mutually exclusive `Fix` alternatives for diagnostics
    /// with several valid resolutions (RFC 0015 D2). Empty = none.
    pub suggestions: Vec<Suggestion>,
    /// Secondary locations that explain the primary one (RFC 0009) — e.g.
    /// an unterminated string's opening quote, far from where the failure
    /// surfaces. The LSP maps these to spec-native
    /// `DiagnosticRelatedInformation`; the CLI prints `note:` lines.
    pub related: Vec<Related>,
}

/// One secondary location on a [`Diagnostic`] — see [`Diagnostic::related`].
#[derive(Debug, Clone)]
pub struct Related {
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code: None,
            severity,
            message: message.into(),
            span: None,
            source: None,
            suggestions: Vec::new(),
            related: Vec::new(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(Severity::Error, message)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(Severity::Warning, message)
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self::new(Severity::Info, message)
    }

    pub fn with_code(mut self, code: Code) -> Self {
        self.code = Some(code);
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn with_related(mut self, span: Span, message: impl Into<String>) -> Self {
        self.related.push(Related {
            span,
            message: message.into(),
        });
        self
    }

    /// Attach a singular near-miss correction ([`SuggestionKind::DidYouMean`]).
    pub fn with_suggestion(mut self, replacement: impl Into<String>, span: Span) -> Self {
        self.suggestions.push(Suggestion {
            replacement: replacement.into(),
            span,
            kind: SuggestionKind::DidYouMean,
        });
        self
    }

    /// Attach one of N mutually exclusive fix alternatives
    /// ([`SuggestionKind::Fix`]) — call once per alternative.
    pub fn with_fix(mut self, replacement: impl Into<String>, span: Span) -> Self {
        self.suggestions.push(Suggestion {
            replacement: replacement.into(),
            span,
            kind: SuggestionKind::Fix,
        });
        self
    }

    /// The human-facing message as a zero-allocation [`fmt::Display`]
    /// adapter (the `Path::display()` pattern): `message` plus the derived
    /// `(did you mean "…"?)` hint when a machine-applicable suggestion
    /// exists. The one renderer every surface shares (`Display`, the CLI,
    /// the LSP) — producers carry the suggestion structurally and never bake
    /// prose hints into `message` by hand.
    pub fn rendered(&self) -> Rendered<'_> {
        Rendered(self)
    }

    /// [`Self::rendered`] as an owned `String`, for consumers that need one
    /// (the LSP wire's `message` field, test assertions).
    pub fn rendered_message(&self) -> String {
        self.rendered().to_string()
    }
}

/// See [`Diagnostic::rendered`].
pub struct Rendered<'a>(&'a Diagnostic);

/// Write `text` with control characters escaped (`\n` → `\u{a}`-style),
/// still zero-alloc. Diagnostics echo *untrusted source text* (found
/// tokens, bad literals, enum values); a malicious file must not be able
/// to smuggle terminal escape sequences into CLI output or log lines.
/// One choke point — every render path goes through [`Rendered`], so no
/// producer has to remember.
fn write_sanitized(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    use fmt::Write as _;
    for ch in text.chars() {
        if ch.is_control() {
            write!(f, "{}", ch.escape_default())?;
        } else {
            f.write_char(ch)?;
        }
    }
    Ok(())
}

impl fmt::Display for Rendered<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_sanitized(f, &self.0.message)?;
        let dym: Vec<&Suggestion> = self
            .0
            .suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::DidYouMean)
            .collect();
        let fixes: Vec<&Suggestion> = self
            .0
            .suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::Fix)
            .collect();
        match dym.as_slice() {
            [] => {}
            [one] => {
                f.write_str(" (did you mean \"")?;
                write_sanitized(f, &one.replacement)?;
                f.write_str("\"?)")?;
            }
            many => {
                f.write_str(" (did you mean one of ")?;
                for (i, s) in many.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str("\"")?;
                    write_sanitized(f, &s.replacement)?;
                    f.write_str("\"")?;
                }
                f.write_str("?)")?;
            }
        }
        // Fix alternatives render capped — the message already states the
        // resolution space; this is a preview, not a re-enumeration.
        const RENDERED_FIXES: usize = 3;
        match fixes.as_slice() {
            [] => {}
            // An empty replacement is a deletion fix — say so instead of
            // rendering an empty backtick pair.
            [one] if one.replacement.is_empty() => {
                f.write_str(" (fix: remove)")?;
            }
            [one] => {
                f.write_str(" (fix: `")?;
                write_sanitized(f, &one.replacement)?;
                f.write_str("`)")?;
            }
            many => {
                f.write_str(" (fixes: ")?;
                for (i, s) in many.iter().take(RENDERED_FIXES).enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str("`")?;
                    write_sanitized(f, &s.replacement)?;
                    f.write_str("`")?;
                }
                if many.len() > RENDERED_FIXES {
                    write!(f, ", … and {} more", many.len() - RENDERED_FIXES)?;
                }
                f.write_str(")")?;
            }
        }
        Ok(())
    }
}

/// Display: `[<source>: ]<severity>[<code>]: <rendered message>[ [start..end]]`
/// — e.g. `error[NML2000]: invalid value … (did you mean "warn"?)`.
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(source) = &self.source {
            write!(f, "{source}: ")?;
        }
        write!(f, "{}", self.severity)?;
        if let Some(code) = self.code {
            write!(f, "[{code}]")?;
        }
        write!(f, ": {}", self.rendered())?;
        if let Some(span) = self.span {
            write!(f, " [{}..{}]", span.start, span.end)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_constants_are_unique_and_in_band() {
        let mut seen = std::collections::HashMap::new();
        for (name, code) in codes::ALL {
            if let Some(prev) = seen.insert(code.0, name) {
                panic!("code {} reused by {prev} and {name}", code);
            }
            // The allocated code space (bands are contiguous — see the
            // `codes` module doc — so the space bound is the checkable
            // invariant; per-band placement is a review concern).
            assert!(
                matches!(code.0, 1..=5999),
                "{name} = {} is outside the allocated code space",
                code
            );
        }
    }

    #[test]
    fn code_display_is_zero_padded() {
        assert_eq!(codes::DUPLICATE_DECLARATION.to_string(), "NML1000");
        assert_eq!(codes::UNKNOWN_TEMPLATE_NAMESPACE.to_string(), "NML5004");
    }

    #[test]
    fn display_without_span_or_code() {
        let diag = Diagnostic::error("something went wrong");
        assert_eq!(diag.to_string(), "error: something went wrong");
    }

    #[test]
    fn display_with_span_and_code() {
        let diag = Diagnostic::warning("looks odd")
            .with_code(codes::UNKNOWN_PROPERTY)
            .with_span(Span::new(4, 17));
        assert_eq!(diag.to_string(), "warning[NML2001]: looks odd [4..17]");
    }

    /// Kind-aware rendering: N mutually exclusive fixes render capped, and a
    /// singular did-you-mean stays byte-identical to the historical form.
    #[test]
    fn rendered_message_renders_fix_alternatives_capped() {
        let mut d = Diagnostic::error("ambiguous").with_span(Span::new(0, 4));
        for v in ["a", "b", "c", "d"] {
            d = d.with_fix(format!("slot as {v}"), Span::new(0, 4));
        }
        let out = d.rendered_message();
        assert!(
            out.contains("(fixes: `slot as a`, `slot as b`, `slot as c`, … and 1 more)"),
            "{out}"
        );
        let single = Diagnostic::error("x").with_fix("slot as a", Span::new(0, 4));
        assert!(single.rendered_message().contains("(fix: `slot as a`)"));
    }

    #[test]
    fn rendered_message_derives_hint_from_suggestion() {
        let diag =
            Diagnostic::error("invalid value \"wran\"").with_suggestion("warn", Span::new(1, 5));
        assert_eq!(
            diag.rendered_message(),
            "invalid value \"wran\" (did you mean \"warn\"?)"
        );
        assert_eq!(
            diag.to_string(),
            "error: invalid value \"wran\" (did you mean \"warn\"?)"
        );
    }

    #[test]
    fn error_index_is_lf_only() {
        // `.gitattributes` pins LF at checkout so the `include_str!` bytes
        // are platform-stable; a CRLF checkout silently breaks the `"\n\n"`
        // paragraph splitter in `summary_of`. Fail by name here rather than
        // obliquely in every derivation test.
        assert!(
            !ERROR_INDEX.contains('\r'),
            "error-index.md was checked out with CRLF line endings — the \
             repo-root .gitattributes (`* text=auto eol=lf`) should prevent \
             this; renormalize the checkout"
        );
    }

    #[test]
    fn explain_summary_is_first_paragraph_with_links_grounded() {
        // The meaning paragraph only — never the example blocks.
        let s = explain_summary("NML2007").expect("known code");
        assert!(s.starts_with("**Missing required field.**"), "{s}");
        assert!(!s.contains("```"), "no examples in a summary: {s}");

        // NML0001's first paragraph carries a relative link — stripped to
        // its text so nothing dangles in hover context.
        let s = explain_summary("NML0001").expect("known code");
        assert!(s.contains("stability policy"), "{s}");
        assert!(!s.contains("](../"), "relative links must be stripped: {s}");

        // Unknown codes are None — same contract as `explain`.
        assert!(explain_summary("NML9999").is_none());

        // Every coded section yields a non-empty summary (the hover surface
        // covers the whole index by construction).
        for (_, code) in codes::ALL {
            let s = explain_summary(&code.to_string())
                .unwrap_or_else(|| panic!("{code} has no summary"));
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn strip_relative_links_keeps_absolute_ones() {
        assert_eq!(
            strip_relative_links("see [the policy](../stability.md) and [site](https://nml.dev)"),
            "see the policy and [site](https://nml.dev)"
        );
        // Unmatched shapes pass through verbatim, never mangled.
        assert_eq!(strip_relative_links("a [lone bracket"), "a [lone bracket");
        assert_eq!(strip_relative_links("no links at all"), "no links at all");
    }

    #[test]
    fn explain_document_composes_canonical_head_and_full_body() {
        // The heading is the MATCHED section head — canonical, never the
        // caller's string (injection-proof by construction).
        let doc = explain_document("NML0001").expect("known code");
        assert!(doc.starts_with("# NML0001\n\n"), "{doc}");
        // Full entry: the example fences that summaries exclude are here.
        assert!(doc.contains("```"), "full body includes examples: {doc}");
        // Relative links are stripped outside fences (NML0001's stability-
        // policy link), same policy as hover summaries.
        assert!(doc.contains("stability policy"), "{doc}");
        assert!(!doc.contains("](../"), "relative links stripped: {doc}");

        // Unknown and hostile inputs are None — nothing the caller sends can
        // reach output (exact-match lookup against vetted heads only).
        assert!(explain_document("NML9999").is_none());
        assert!(explain_document("../../etc/passwd").is_none());
        assert!(explain_document("NML0001\n\n# forged heading").is_none());

        // Total over the code space, like `explain` itself.
        for (_, code) in codes::ALL {
            assert!(explain_document(&code.to_string()).is_some(), "{code}");
        }
    }

    #[test]
    fn explain_document_never_rewrites_fenced_content() {
        // A fenced line shaped like a relative link must pass through
        // verbatim — fences are content, not prose. (Synthetic: today's
        // index has no fenced links; this pins the composer's behavior,
        // and `index_sections_are_fence_safe` pins the content invariant.)
        let body = "prose [x](./rel.md)\n\n```\ncode [y](./rel.md)\n```\n";
        let composed = compose_document("NML0000", body);
        assert!(composed.starts_with("# NML0000\n\n"), "{composed}");
        assert!(composed.contains("prose x\n"), "{composed}");
        assert!(composed.contains("code [y](./rel.md)\n"), "{composed}");
    }

    #[test]
    fn explain_index_lists_every_code_with_its_summary() {
        let index = explain_index();
        // Bidirectional over the code space: every code appears exactly once,
        // every entry is a real code with a non-empty summary, order is the
        // index's own (band-ascending) order.
        assert_eq!(index.len(), codes::ALL.len(), "index ↔ codes drift");
        for (head, summary) in &index {
            assert!(
                head.len() == 7
                    && head.starts_with("NML")
                    && head[3..].bytes().all(|b| b.is_ascii_digit()),
                "malformed head {head:?}"
            );
            assert!(!summary.is_empty(), "{head} has an empty summary");
            assert_eq!(explain_summary(head).as_deref(), Some(summary.as_str()));
        }
        let mut heads: Vec<_> = index.iter().map(|(h, _)| *h).collect();
        heads.dedup();
        assert_eq!(heads.len(), index.len(), "duplicate section heads");
    }

    #[test]
    fn index_sections_are_fence_safe() {
        // A fenced line starting `## ` would silently truncate a section for
        // EVERY consumer (the splitter is not fence-aware by design — this
        // tripwire is the cheaper structural guarantee). Fences must also
        // balance, or the composer's fence tracking would invert.
        let mut in_fence = false;
        for line in ERROR_INDEX.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                assert!(
                    !line.starts_with("## "),
                    "fenced heading would truncate a section: {line:?}"
                );
            }
        }
        assert!(!in_fence, "unbalanced code fence in the error index");
    }

    #[test]
    fn rendered_escapes_control_characters() {
        // Diagnostics echo untrusted source text; a malicious file must not
        // smuggle terminal escapes into CLI output through ANY render path.
        let d = Diagnostic::error("bad \u{1b}[31mred\u{7} value".to_string());
        let out = d.rendered_message();
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
        assert!(out.contains("\\u{1b}"), "escaped visibly: {out:?}");
    }

    #[test]
    fn explain_covers_every_code_and_rejects_unknowns() {
        for (name, code) in codes::ALL {
            assert!(
                explain(&code.to_string()).is_some(),
                "{name} ({code}) has no error-index section"
            );
        }
        assert!(explain("NML9999").is_none());
        assert!(explain("nonsense").is_none());
    }

    #[test]
    fn info_severity_displays() {
        assert_eq!(Diagnostic::info("fyi").to_string(), "info: fyi");
    }
}
