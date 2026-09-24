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
#[non_exhaustive]
pub enum SuggestionKind {
    DidYouMean,
    Fix,
    /// Structural deletion of the syntax node whose content span equals
    /// `span`: a body entry (`BodyEntry.span`), a `uses` clause
    /// (`BlockDecl.uses_span`), or a clause reference (`Identifier.span`).
    /// `replacement` is always empty. Singular and machine-applicable
    /// (DidYouMean's exclusivity, RFC 0017 §4.1); the bytes are computed
    /// only by [`resolve_suggestions`](crate::cst::edit::resolve_suggestions)
    /// — never by textual widening. Renders nothing in the message: the
    /// producer's prose states the action.
    Delete,
    /// Structural insertion of the body entries `replacement` spells — a
    /// snippet written at zero indentation, its newlines its structure —
    /// as the LAST entry of the block, list item or declaration whose
    /// NAME token span equals `span` (a remedy block: NML2064's `layers:`
    /// grant under the binding). Singular and exact; the bytes — the
    /// snippet at the body's own indentation, at the offset after its
    /// last entry — are computed only by `resolve_suggestions` against
    /// the target file's text, so every applier and every wire carries
    /// the one edit; a block already holding an entry of the snippet's
    /// head name is refused (`AlreadyPresent`) — an insertion never
    /// doubles one. Renders nothing in the message: the CLI prints the
    /// resolved block beneath the finding as its `help:`, the editor
    /// offers it as a quick-fix on the file it edits.
    Insert,
}

impl SuggestionKind {
    /// The LSP `data.suggestions[].kind` string — exhaustive HERE, in the
    /// defining crate, where a new variant cannot compile without naming
    /// itself (`#[non_exhaustive]` would force a wildcard arm on an
    /// external matcher and lose that forcing).
    pub fn wire_name(self) -> &'static str {
        match self {
            SuggestionKind::DidYouMean => "didYouMean",
            SuggestionKind::Fix => "fix",
            SuggestionKind::Delete => "delete",
            SuggestionKind::Insert => "insert",
        }
    }

    /// The inverse of [`Self::wire_name`], for the editor's code action —
    /// an unknown string is no action, never a guess.
    pub fn from_wire_name(s: &str) -> Option<Self> {
        match s {
            "didYouMean" => Some(SuggestionKind::DidYouMean),
            "fix" => Some(SuggestionKind::Fix),
            "delete" => Some(SuggestionKind::Delete),
            "insert" => Some(SuggestionKind::Insert),
            _ => None,
        }
    }
}

/// A machine-applicable edit carried alongside a diagnostic (RFC 0030): the
/// exact replacement text and the exact span it replaces. Produced wherever
/// a correction is *derivable* (e.g. a did-you-mean), so editors can offer a
/// one-keystroke quick-fix instead of leaving the suggestion trapped in
/// message prose — and a remedy the reader pastes (rustc's `help:` with a
/// suggested replacement) is one of these, never prose beside the finding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Suggestion {
    /// The text to insert at `span` (for a string value: the bare content,
    /// without quotes — `span` covers the string's content, not its quotes).
    /// For an [`Insert`](SuggestionKind::Insert): the entries to add, at
    /// zero indentation.
    pub replacement: String,
    /// The exact range the replacement substitutes; for a structural kind
    /// the anchor the resolver relocates (a deletion's node, an insertion's
    /// named block).
    pub span: Span,
    /// The exclusivity semantics — see [`SuggestionKind`].
    pub kind: SuggestionKind,
    /// The file `span` indexes into, when it differs from the
    /// diagnostic's own (`Diagnostic.source` vocabulary — a path);
    /// `None` inherits the diagnostic's own
    /// ([`Diagnostic::suggestion_source`]). An applier edits ONLY the file
    /// it was asked to — a content file's finding may carry the edit an
    /// operator applies to the manifest, and that edit is routed there,
    /// never resolved against the content file's text.
    pub source: Option<String>,
}

/// A suggestion's kind and payload before its anchor: the ONE way to build
/// a [`Suggestion`] is `Suggestion::<kind>(payload).at(span)` — the payload
/// arrives with the kind that gives it meaning, the span is REQUIRED by the
/// type (there is no suggestion without one, and nothing else turns an
/// `Unanchored` into a `Suggestion`), the file is optional and named
/// ([`Suggestion::in_file`]). Two producers can never spell one builder
/// with two argument orders: every fact has a name, none a position.
///
/// # Examples
///
/// ```
/// use nml_core::diagnostic::{Diagnostic, Suggestion, SuggestionKind};
/// use nml_core::span::Span;
///
/// // `hots = …` at bytes 12..16, where the model spells `host`.
/// let fix = Suggestion::did_you_mean("host").at(Span::new(12, 16));
/// assert_eq!(fix.kind, SuggestionKind::DidYouMean);
/// assert_eq!(fix.replacement, "host");
/// assert!(fix.source.is_none(), "the diagnostic\'s own file, until named");
///
/// // A content file\'s finding may carry the edit an operator applies to
/// // the manifest: `in_file` routes it there.
/// let elsewhere = Suggestion::insert("layers:\n    allowRefs:\n        - \"a\"")
///     .at(Span::new(0, 0))
///     .in_file("demo.package.nml");
/// assert_eq!(elsewhere.source.as_deref(), Some("demo.package.nml"));
///
/// let diag = Diagnostic::error("unknown property \'hots\'".to_string()).with_suggestion(fix);
/// assert_eq!(diag.suggestions.len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an unanchored suggestion is not a suggestion: call `.at(span)`"]
pub struct Unanchored {
    kind: SuggestionKind,
    replacement: String,
}

impl Unanchored {
    /// Anchor the suggestion: the exact range a verbatim replacement
    /// substitutes; for a structural kind the node (a deletion) or the
    /// NAME token (an insertion) the resolver relocates. The file is the
    /// diagnostic's own until [`Suggestion::in_file`] names another.
    pub fn at(self, span: Span) -> Suggestion {
        Suggestion {
            replacement: self.replacement,
            span,
            kind: self.kind,
            source: None,
        }
    }
}

impl Suggestion {
    /// A singular near-miss correction ([`SuggestionKind::DidYouMean`]).
    pub fn did_you_mean(replacement: impl Into<String>) -> Unanchored {
        Self::of(SuggestionKind::DidYouMean, replacement)
    }

    /// One of N mutually exclusive alternatives ([`SuggestionKind::Fix`])
    /// — one suggestion per alternative.
    pub fn fix(replacement: impl Into<String>) -> Unanchored {
        Self::of(SuggestionKind::Fix, replacement)
    }

    /// A structural deletion ([`SuggestionKind::Delete`]) of the node
    /// whose content span the anchor names — it carries no text.
    pub fn delete() -> Unanchored {
        Self::of(SuggestionKind::Delete, String::new())
    }

    /// A structural insertion ([`SuggestionKind::Insert`]) of the entries
    /// `entries` spells (zero indentation) as the last of the body whose
    /// owner's NAME token the anchor names.
    pub fn insert(entries: impl Into<String>) -> Unanchored {
        Self::of(SuggestionKind::Insert, entries)
    }

    /// A kind with its payload — the four named entries above are this
    /// one's spellings, and a reader that holds a kind from a wire
    /// ([`SuggestionKind::from_wire_name`]) comes in here. A deletion
    /// carries no text whatever the wire said: its bytes are the
    /// resolver's, never a payload's.
    pub fn of(kind: SuggestionKind, replacement: impl Into<String>) -> Unanchored {
        let replacement = match kind {
            SuggestionKind::Delete => String::new(),
            _ => replacement.into(),
        };
        Unanchored { kind, replacement }
    }

    /// The file the anchor indexes into, when it is not the diagnostic's
    /// own (`Diagnostic.source` vocabulary — a path): an applier routes
    /// the edit there and never resolves it against another file's text.
    pub fn in_file(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}

/// A stable diagnostic code (`NML0042`).
///
/// The inner number is private: codes are constructible only from the vetted
/// constants in [`codes`], so "never renumbered, never reused" is a compile
/// guarantee, not a convention. [`fmt::Display`] is the only accessor — both
/// consumers (the CLI's `error[NML0042]` prefix and the LSP's string `code`
/// field) want the formatted form; a numeric getter would be speculative API
/// until something needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code(u16);

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NML{:04}", self.0)
    }
}

/// Declares the code constants plus the two derived lists — the numbers
/// (always, for the compile-time allocation guard) and the named pairs (in
/// tests, for coverage sweeps) — from one source, so neither can drift
/// from the declarations.
macro_rules! codes {
    ($($(#[$doc:meta])* $name:ident = $num:literal;)+) => {
        $($(#[$doc])* pub const $name: Code = Code($num);)+
        /// **The allocation guard, enforced at compile time.** Declarations are
        /// strictly ascending — one invariant that buys three properties, and
        /// the reason it is ordering rather than mere uniqueness:
        ///
        /// * **Reuse is impossible.** Strictly increasing implies distinct, so
        ///   the "never reused" half of the stability contract is a compile
        ///   error rather than a test failure — the code cannot be built, let
        ///   alone shipped.
        /// * **The next free code is readable.** It is one past a band's last
        ///   entry, visible at a glance. Three consecutive allocation
        ///   collisions (a proposed pair already taken, then a second pair
        ///   whose lower half was taken by an out-of-order entry) all traced to
        ///   the same cause: the list was unordered, so "what is free?" meant
        ///   scanning 200 lines, and scanning misses.
        /// * **Insertion is self-locating.** A new code goes beside its
        ///   numeric neighbours, so a mistake is visible on the line being
        ///   typed rather than in a distant summary.
        ///
        /// Gaps are fine and expected (`2056` is one — a retired allocation
        /// from a corrected collision): bands are allocation convenience, not
        /// API, and nothing enumerates a contiguous range. Closing a gap would
        /// mean renumbering, which the contract forbids.
        const _: () = {
            const DECLARED: &[u16] = &[$($num),+];
            let mut i = 0;
            while i < DECLARED.len() {
                assert!(
                    DECLARED[i] >= 1 && DECLARED[i] <= 5999,
                    "diagnostic code is outside the allocated space (1..=5999)"
                );
                assert!(
                    i == 0 || DECLARED[i - 1] < DECLARED[i],
                    "diagnostic codes must be declared in strictly ascending order \
                     (this also proves none is reused) — move the new code beside \
                     its numeric neighbours; the next free code in a band is one \
                     past that band's last entry"
                );
                i += 1;
            }
        };
        #[cfg(test)]
        pub(crate) const ALL: &[(&str, Code)] = &[$((stringify!($name), $name)),+];
    };
}

/// The stable code space, banded by subsystem for allocation convenience
/// (bands are **not** API — a diagnostic moving between subsystems keeps its
/// code): 0001–0999 lex/parse · 1000–1999 symbols & resolution · 2000–2999
/// schema loading & validation · 3000–3999 values, money & durations ·
/// 4000–4999 packages & store · 5000–5999 editor/LSP. Sites not yet assigned a code
/// emit `None`; the docs plan's Phase 4 sweep completes coverage. Every code
/// has a section in the error index (`docs/errors/README.md`) — enforced
/// bidirectionally by `just gate-docs`.
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
        /// A number outside the exact decimal domain (RFC 0016; numbers are exact by design).
        NUMBER_OUT_OF_RANGE = 14;
        /// A malformed `$NS.key` variable reference.
        BAD_SECRET_REF = 15;
        /// A carriage return with no following line feed (spec: Source
        /// text — line endings are LF or CRLF).
        BARE_CARRIAGE_RETURN = 16;
        /// A raw control character (any Unicode Cc — C0, DEL, or C1 —
        /// minus tab/line endings) in source; the `\u{…}` escape is the
        /// sanctioned spelling.
        FORBIDDEN_CONTROL = 17;
        /// An invisible steering character (bidirectional controls,
        /// interior U+FEFF, the U+2028/U+2029 line separators) — the
        /// Trojan Source defense.
        INVISIBLE_CHARACTER = 18;
        /// Content on a multi-line string's opening line (content must
        /// begin on a new line — the Swift/Java text-block rule).
        MULTILINE_OPENING_CONTENT = 19;
        /// An own-line closing `"""` not aligned with the content's
        /// indentation (machine-fixable: move the delimiter).
        MULTILINE_CLOSING_MISALIGNED = 20;
        /// A fallback chain (`a | b`) in a list position — elements are
        /// single values (chains live at properties or behind `const`
        /// names).
        FALLBACK_IN_LIST_ITEM = 21;
        /// The source exceeds the parser's 4 GiB bound (token positions
        /// are `u32`, as in `rowan`); the tree is empty and this is its
        /// one finding — a resource bound, never a panic.
        SOURCE_TOO_LARGE = 22;

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
        /// Model `extends` chains form a cycle.
        EXTENDS_CYCLE = 2013;
        /// Model references form a cycle (advisory: legal, but often a sign
        /// of an unintended self-reference).
        MODEL_REFERENCE_CYCLE = 2014;
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
        /// **Retired** (RFC 0017): durations are literals now, so format
        /// defects surface at decode as `NML3004`/`NML3005`/`NML3006` and
        /// this validation-time check can no longer fire. The constant
        /// stays declared — codes are never renumbered or reused, and the
        /// uniqueness test enforces that by construction; the error index
        /// keeps its section as a tombstone.
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
        /// A oneof arm carries a plain field named like the discriminator
        /// — unsettable, the property is always claimed as the
        /// discriminator (an error at load; the optional `#sealed`
        /// spelling is the seal and stays).
        SHADOWED_DISCRIMINATOR = 2054;
        /// A list item's BODY has nowhere to go: the element type is a
        /// scalar/union/collection with no fields to fill — the body-side
        /// mirror of the dropped-key rule above.
        DROPPED_ITEM_BODY = 2055;
        /// RFC 0018: a number violates a declared facet (`min`/`max`/
        /// `exclusiveMin`/`exclusiveMax`/`multipleOf`). Exact
        /// comparisons — no epsilon, no float rounding.
        FACET_VIOLATION = 2057;
        /// RFC 0018: an invalid facet declaration — facets on a
        /// non-`number` type, unknown/duplicate/conflicting keys, an
        /// unsatisfiable range, or `multipleOf <= 0`. (A default that
        /// violates its own facets reports as the VIOLATION code
        /// through the shared enforcement pass.)
        FACET_DEFINITION = 2058;
        /// RFC 0019: a `uses` layer ref does not resolve to an in-scope
        /// instance (did-you-mean over in-scope same-keyword instances).
        UNRESOLVED_LAYER_REF = 2059;
        /// RFC 0019: a `#sealed` field a lower layer already fixed is
        /// violated — by a differing assignment, an equal-value restatement
        /// (drift hazard), or a variant switch discarding the sealed body
        /// (the seal backstop).
        SEALED_FIELD_VIOLATION = 2060;
        /// RFC 0019: a `uses` reference cycle.
        LAYER_CYCLE = 2061;
        /// RFC 0019: a `uses` target declares a different model keyword than
        /// the composing block, or a `uses` clause sits on a schema
        /// definition (`model`/`trait`/`enum`).
        LAYER_KEYWORD_MISMATCH = 2062;
        /// RFC 0019: illegal identity redefinition — under `#append` without
        /// `#identity`; a cross-kind match at an equal token; replacing a
        /// bodiless reference/role item; or a duplicate identity within one
        /// layer's list.
        IDENTITY_REDEFINITION = 2063;
        /// RFC 0019: composition not permitted — the governing binding has no
        /// `layers:` grant, the file is ambiguously claimed, or no binding
        /// governs it in a closed universe.
        COMPOSITION_DENIED = 2064;
        /// RFC 0019: a `uses` ref denied by the layer grant — a `denyRefs`
        /// veto (named by index) or an allow-miss (no `allowRefs` entry
        /// admits the layer).
        LAYER_REF_DENIED = 2065;
        /// RFC 0019: a composition bound exceeded — the grant's
        /// `maxStackDepth`, the language stack cap (16), or the
        /// import-closure cap (256 files). The message names which.
        LAYER_BOUND_EXCEEDED = 2066;
        /// RFC 0019: an overlay item matches no base identity in an
        /// `#identity` list without `#append` (did-you-mean over the base's
        /// NAMED identities only — scalar-keyed tokens are never echoed).
        UNMATCHED_OVERLAY_ITEM = 2067;
        /// RFC 0019: invalid merge-policy declaration at schema load —
        /// `#identity` with no mergeable identity (plain scalar lists,
        /// `set<T>`), or an incoherent combination (`#sealed` with any
        /// other; list policies on non-collections).
        INVALID_MERGE_POLICY = 2068;
        /// RFC 0019 (warning): a seal that cannot engage — `#sealed` item
        /// fields under a bare-overlay list, a oneof (field-typed or
        /// instance-rooted) with sealed arm fields and an unsealed
        /// discriminator, or `#sealed` on a field with a schema default.
        UNREACHABLE_SEAL = 2076;
        /// RFC 0019: no consistent linearization — the `uses` DAG's declared
        /// orders contradict (C3 merge failure).
        INCONSISTENT_LINEARIZATION = 2077;
        /// RFC 0019 (warning): a composing layer's list entry normalizes to
        /// zero items — it does not supply the list, and "empty the base
        /// list" has no merge spelling.
        ZERO_ITEM_LAYER_ENTRY = 2079;
        /// RFC 0019 (warning): a project config, package manifest or root
        /// marker inside content another manifest's binding claims is
        /// inert — content, not configuration (resolution inputs must not
        /// be author-writable).
        INERT_RESOLUTION_INPUT = 2080;
        /// RFC 0019 item 4: a validator binding's `layers:` grant breaks a
        /// rule of the loader's own — an `allowRefs`/`denyRefs` glob the
        /// matcher rejects (`**` must be a whole segment; the segment cap),
        /// `maxStackDepth` above the language cap or not a whole number, or
        /// `denyRefs` beside an empty `allowRefs` — refused at manifest LOAD,
        /// located at the item. Load-time because an over-cap glob matches
        /// nothing: fail-closed for an allow rule, fail-OPEN for a deny.
        LAYER_GRANT_RULE = 2081;
        /// RFC 0019: a package manifest's `[]directive` vocabulary declares
        /// one of the language's merge-policy directives (`sealed`,
        /// `identity`, `append`, `overlay`) — reserved names, refused at
        /// manifest LOAD at the entry.
        RESERVED_DIRECTIVE = 2082;
        /// RFC 0019: a closed binding rejects content reached through a
        /// symlinked path component (pipeline P4) — form 1: a component
        /// is a symlink; form 2: the on-disk spelling cannot be verified
        /// on this backend.
        SYMLINKED_CONTENT_REJECTED = 2083;
        /// RFC 0019 (warning): an overlay assignment restates the effective
        /// lower value unchanged (`semantic_eq`) — a dead delta. Overlay- or
        /// sealed-policy scalar/object fields only.
        DEAD_DELTA = 2084;
        /// RFC 0015+0019: a union-typed position discarded a layer's
        /// contribution that can neither merge into the established
        /// variant nor switch it — a whole-value (structural) spelling
        /// over an established named variant, or an un-annotated body
        /// over an established structural value (only an authored `as`
        /// switches). Loud by design: silence here is data loss.
        DISCARDED_UNION_CONTRIBUTION = 2085;
        /// An internal composition invariant was violated (a decision the
        /// engine believes unreachable was reached). The layer's
        /// contribution is NOT composed — fail safe and loud, never
        /// silently wrong. Please report the input.
        INTERNAL_COMPOSE_INVARIANT = 2086;
        /// RFC 0019 item 0 (rule 3): two or more live workspace manifests
        /// claim one file — denied; the file validates under no binding
        /// (never a nearest-wins shadow).
        AMBIGUOUS_CLAIM = 2087;
        /// RFC 0019 item 0 (E27(3)): a LIVE package manifest or project
        /// config could not be loaded — unreadable, malformed, over its
        /// byte cap, a declared source unavailable — so the universe is
        /// closed-denied: nothing under it validates.
        RESOLUTION_INPUT_UNLOADABLE = 2088;
        /// RFC 0019 item 0 (A16): discovery could not enumerate part of
        /// the universe — the entry bound was reached or a directory was
        /// unreadable — so what it could not enumerate is closed-denied:
        /// the whole universe when the root unit or the universe-wide
        /// backstop is spent, and exactly one budget unit's subtree when
        /// a tenant-shaped unit spends its own bound.
        UNIVERSE_TRUNCATED = 2089;
        /// RFC 0019 item 0: the gate over a directory found
        /// `.nml` content the universe walk skipped by policy — a symlink
        /// in an open universe, a FIFO, a dot-file, a file under a
        /// dot-directory, a hidden directory too large to audit whole —
        /// content a runtime could read that no verb judged: an error
        /// the walking verbs fail on. A symlink the walk left whose name
        /// is not `.nml`-shaped is named as a warning (what lies beneath
        /// it is unknown). A symlinked `.nml` in a CLOSED universe is
        /// NML2083, in the resolver's words.
        UNJUDGED_CONTENT = 2090;
        /// RFC 0019 item 0: the binding that governs a file cannot build
        /// its validator — a declared schema source fails to load (a
        /// parse error, a duplicate definition, a cycle) — so the file
        /// validates under NO binding: an error on every file the binding
        /// governs, naming the source's first finding; the manifest's
        /// other bindings are unaffected, and the source document itself
        /// keeps its own findings (it is where the operator repairs it).
        VALIDATOR_UNBUILDABLE = 2091;
        /// RFC 0019 item 4 (E38): a binding glob whose first wildcard
        /// directory run is not its last (`tenants/*/flows/**`) delegates
        /// content shallower than the budget unit inferred from its LAST
        /// run, so the content between — `tenants/<x>/other/` — is the
        /// root unit's, where one tenant's flood denies everyone. A
        /// warning on the manifest, at the glob, silenced by an explicit
        /// `budgetUnits` declaration (which replaces the inference).
        BUDGET_UNIT_GAP = 2092;
        /// A body declares the same name twice — `version = "1"` twice,
        /// or a block `files:` beside an inline `files = […]` (two
        /// spellings of one entry) — an error at the later occurrence,
        /// the first a related note; no suggestion (which is meant is
        /// unknowable). A manifest's `[]schema` and `[]validator` items
        /// are named entries too (a binding is its name). Structural:
        /// schema or not, before any merge.
        DUPLICATE_ENTRY = 2093;
        /// RFC 0030: a package manifest declares its `package` block and
        /// each of its `[]schema`, `[]validator` and `[]directive` arrays
        /// once — one slot per keyword, whatever the names; a second
        /// declaration of one keyword is refused at the later keyword, the
        /// first a related note. A loader rule: the manifest's NML2088
        /// row carries it as its cause. (The SAME name twice is NML1000,
        /// the file-scope rule, refused where the manifest is parsed.)
        REPEATED_DECLARATION = 2094;
        /// RFC 0030: a manifest needs a `package <name>:` block and at
        /// least one `[]schema` source — a package with nothing to bind is
        /// refused at load, never loaded as empty. A loader rule (cause
        /// of the manifest's NML2088 row).
        MISSING_DECLARATION = 2095;
        /// RFC 0030: a package's name is a lowercase identifier
        /// (`[a-z][a-z0-9-]*`) — it becomes a store path component and a
        /// written pin entry. A loader rule (cause of NML2088).
        INVALID_PACKAGE_NAME = 2096;
        /// RFC 0030: a `[]schema`, `[]validator` or `[]directive` entry is
        /// its name (`- core:` with a body), never a quoted or positional
        /// item: a source is what a binding's `schemas` names, a binding is
        /// what `nml binding` prints. A loader rule (cause of NML2088).
        UNNAMED_ENTRY = 2097;
        /// RFC 0030: a validator binding claims files and names schemas,
        /// both non-empty — a binding with `files = []` or `schemas = []`
        /// binds nothing, a mistake rather than a no-op. A loader rule
        /// (cause of NML2088).
        EMPTY_BINDING = 2098;
        /// RFC 0030: a binding's `schemas` entries name the manifest's own
        /// `[]schema` sources by logical name; a name no entry declares
        /// is a dangling reference. A loader rule (cause of NML2088).
        UNDECLARED_SCHEMA = 2099;
        /// RFC 0019 item 0: a binding's `files` glob the matcher rejects
        /// (`**` not a whole segment, a segment that names no key
        /// component, over the segment cap) matches nothing — a binding
        /// that silently claims nothing — so it is refused at load, at the
        /// glob. A loader rule (cause of NML2088); the same rule over a
        /// grant's `allowRefs`/`denyRefs` is NML2081.
        INVALID_BINDING_GLOB = 2100;
        /// RFC 0019 item 4 (RFC 0026 B-3): a declared `budgetUnits` entry
        /// that is not a directory pattern (1 to the segment cap of
        /// `/`-separated plain names or `*`, never `**`), or a declared unit
        /// nesting inside another without a literal pinning one of the
        /// outer unit's wildcards (`tenants/*` beside `tenants/*/*`) —
        /// every directory the outer unit delegates would mint units of
        /// its own beneath it. A loader rule (cause of NML2088), at
        /// `budgetUnits`.
        BUDGET_UNIT_RULE = 2101;
        /// RFC 0030: the manifest's `formatVersion` is newer than the
        /// package format this build understands — the compatibility gate,
        /// checked BEFORE meta-validation so a newer publisher degrades an
        /// older reader with one precise refusal, never a wall of
        /// unknown-key findings. Cause of the manifest's NML2088 row; the
        /// editor's store reports it for an installed package.
        UNSUPPORTED_FORMAT_VERSION = 2102;
        /// RFC 0030: a package directory (the editor's store slot) holds
        /// more than one `<name>.package.nml` — it cannot say which package
        /// it is. No CLI verb loads a package directory, so no transcript
        /// demonstrates it; the loader's own test does.
        MULTIPLE_MANIFESTS = 2103;
        /// A template string (`"tenants/{{x}}"`) as an element of a
        /// manifest list — `files`, `schemas`, `allowRefs`, `denyRefs`,
        /// `budgetUnits`, `rootMarkers`, `modifiers`, `memberKeywords`,
        /// `builtinRefs` hold plain literals naming files, keys, units or
        /// markers. The meta-schema admits a template as a `string`; the
        /// loader used to set it aside silently (a narrower `files`, a
        /// dropped unit, a `denyRefs` veto that never fired). Refused at
        /// the element through the loader's one list gate. A loader rule
        /// (cause of the manifest's NML2088 row).
        TEMPLATE_IN_LIST = 2104;
        /// RFC 0019 item 0: a `[]schema` entry declares a `file` that is not
        /// spelled as a schema source (`*.model.nml`, `*.schema.nml`). That
        /// suffix is the ONE admission every reader shares — the walk that
        /// finds an undeclared source beside a manifest, the `--schema`
        /// directory scan, and the editor, which gates its registry, its
        /// schema passes, completion and hover on the spelling — so a source
        /// declared outside it was a schema to the loader and to nobody else:
        /// `nml check` judged its directives while the editor opened no pass,
        /// gave no row and no hover on the same buffer. The manifest's SHAPE,
        /// refused at load, at the `file` value. A loader rule (cause of the
        /// manifest's NML2088 row).
        INVALID_SCHEMA_SOURCE_NAME = 2105;

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
        /// A number's trailing identifier is neither a currency code nor a
        /// duration unit (`30x`, `30S`, `30sec`); near-miss units get a
        /// machine-applicable suggestion (RFC 0017).
        UNKNOWN_UNIT = 3004;
        /// A duration magnitude with a fractional part (`30.5s`) —
        /// durations are integers; the fix respells the value exactly at
        /// the authored granularity (`30s500ms`) when one exists
        /// (RFC 0017).
        FRACTIONAL_DURATION = 3005;
        /// A duration outside the value domain: negative, or a total
        /// beyond `std::time::Duration::MAX` (durations convert
        /// infallibly by construction — RFC 0017).
        DURATION_OUT_OF_RANGE = 3006;
        /// A compound duration literal repeats the same unit (`1h2h`) —
        /// the fix merges to the canonical form (`3h`).
        DUPLICATE_DURATION_UNIT = 3007;
        /// A compound duration has a dangling magnitude without a unit
        /// suffix (`1h30`, `5m2`).
        MALFORMED_COMPOUND_DURATION = 3008;

        /// A package validator binding is fully shadowed by earlier
        /// bindings — its globs can never match first (RFC 0030).
        SHADOWED_VALIDATOR = 4000;

        /// A directive name is neither a language merge-policy directive nor
        /// in the covering package's declared vocabulary — the kernel's one
        /// judge (`nml_validate::directives::Vocabulary`), every front end.
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

/// A code's one-line HEADLINE — the bold lead its index section opens
/// with (`**Missing required field.** A required …` → `Missing required
/// field.`): the sentence a list or a palette shows where a paragraph
/// would not fit. `nml explain --list` prints it and the editor's
/// explain palette labels with it; the [`explain_summary`] paragraph
/// stays the hover's and the `--json` row's. Every section opens with a
/// bold lead (a unit test holds the index to it); one that did not
/// would headline with its whole summary.
pub fn explain_headline(code: &str) -> Option<String> {
    explain_summary(code).map(|summary| headline_of(&summary))
}

/// The bold lead of a summary, or the summary itself when it opens with
/// none.
fn headline_of(summary: &str) -> String {
    summary
        .strip_prefix("**")
        .and_then(|rest| rest.split_once("**"))
        .map(|(lead, _)| lead.trim().to_string())
        .unwrap_or_else(|| summary.to_string())
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
        if is_fence_delimiter && !in_fence {
            // An OPENING fence keeps its language word and drops the
            // docs-test tags after it (`check expect-error='[…]'`,
            // `transcript=…`): they steer the repository's verification
            // harness and mean nothing to a reader of `nml explain`.
            in_fence = true;
            let indent = &line[..line.len() - line.trim_start().len()];
            let language = line.trim_start()[3..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            doc.push_str(indent);
            doc.push_str("```");
            doc.push_str(language);
            doc.push('\n');
            continue;
        }
        if is_fence_delimiter {
            in_fence = false;
        }
        if in_fence || is_fence_delimiter {
            doc.push_str(line);
        } else {
            doc.push_str(&strip_relative_links(line));
        }
    }
    doc
}

/// Every `(code, summary)` pair **in ascending code order** — the
/// discoverability surface behind the editor's `nml/explainIndex` (the
/// explain-a-code palette) and the CLI's `nml explain --list`. Derived from
/// the index itself rather than [`codes`], so it needs no runtime code
/// enumeration and can never disagree with what [`explain`] serves; the
/// docs-test guard keeps index↔codes bidirectionally complete **and the
/// index's sections ascending**, which is what makes the order here a
/// guarantee callers may rely on rather than an accident of file layout
/// (a palette listing codes out of order is a palette people scroll past).
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
    // Inline code spans are content, not markup: `[](a | b)` inside
    // backticks is a type spelling, not a link. Rewrite only the prose
    // segments (even-indexed after splitting on backticks); fences are
    // handled by the caller.
    let mut out = String::with_capacity(text.len());
    for (i, segment) in text.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 0 {
            out.push_str(&strip_relative_links_in_prose(segment));
        } else {
            out.push_str(segment);
        }
    }
    out
}

fn strip_relative_links_in_prose(text: &str) -> String {
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
    /// with several valid resolutions (RFC 0015 D2); a structural
    /// `Delete` or `Insert` (a remedy block — rustc's `help:` with a
    /// suggested replacement — is one of these, in the file it edits,
    /// never prose beside the finding). Empty = none.
    pub suggestions: Vec<Suggestion>,
    /// Secondary locations that explain the primary one (RFC 0009) — e.g.
    /// an unterminated string's opening quote, far from where the failure
    /// surfaces. The LSP maps these to spec-native
    /// `DiagnosticRelatedInformation`; the CLI prints `note:` lines.
    pub related: Vec<Related>,
    /// The finding whose refusal this one reports, when this finding
    /// wraps another under a code of its own — a manifest that failed
    /// to load (NML2088) over the manifest's first finding, a binding
    /// that cannot build its validator (NML2091) over its declared
    /// source's — so the inner code, sentence and place ride the row as
    /// facts (`cause` on the `--json` row) beside the verdict, never
    /// only inside its sentence. Set by [`Self::caused_by`], which
    /// keeps the rule in one place; `None` for every finding that
    /// wraps nothing.
    pub cause: Option<Box<Cause>>,
}

/// One secondary location on a [`Diagnostic`] — see [`Diagnostic::related`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Related {
    pub span: Span,
    pub message: String,
    /// The file `span` indexes into, when it differs from the
    /// diagnostic's own (`Diagnostic.source` vocabulary — a path);
    /// `None` inherits the diagnostic's own
    /// ([`Diagnostic::related_source`]). Renderers locate a note in ITS
    /// OWN file — a cross-file span through the wrong line index prints
    /// the right file with a wrong range.
    pub source: Option<String>,
}

/// The finding a wrapping [`Diagnostic`] reports the refusal of — see
/// [`Diagnostic::cause`]: its code, its sentence and its place. One
/// level by construction: a cause carries no cause, and
/// [`Diagnostic::caused_by`] takes a wrapped wrapper's own cause — the
/// innermost, the one to act on — never a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cause {
    pub code: Code,
    pub message: String,
    /// Where the finding sits in its file; `None` for a finding with
    /// no place (a renderer then states none).
    pub span: Option<Span>,
    /// The file `span` indexes into (`Diagnostic.source` vocabulary);
    /// `None` inherits the wrapping finding's own
    /// ([`Diagnostic::cause_source`]), as a note's does.
    pub source: Option<String>,
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
            cause: None,
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
            source: None,
        });
        self
    }

    /// [`Self::with_related`] with the note's own file (RFC 0019 plan
    /// item 2) — for a note whose span indexes a different source than
    /// the diagnostic's.
    pub fn with_related_in(
        mut self,
        span: Span,
        message: impl Into<String>,
        source: Option<String>,
    ) -> Self {
        self.related.push(Related {
            span,
            message: message.into(),
            source,
        });
        self
    }

    /// The file `rel`'s span indexes into: the note's own, else the
    /// diagnostic's — the ONE inheritance rule both renderers share.
    pub fn related_source<'s>(&'s self, rel: &'s Related) -> Option<&'s str> {
        self.inherit(rel.source.as_deref())
    }

    /// Report `finding`'s refusal under this finding's own code:
    /// `finding` becomes the cause ([`Cause`]) — its code, sentence and
    /// place, in `source` (`None`: this finding's own file) — so a
    /// consumer reads the underlying code as a fact, and `finding`'s
    /// remedies ride this row ([`Suggestion`]), each stamped with its
    /// file — its own, else `finding`'s, else `source`, else this row's
    /// — so the edit reaches the CLI's `help:` block and `nml fix`'s
    /// routing, and the editor's quick fix, wherever the row is shown
    /// (a manifest's did-you-mean on the row a governed file carries).
    /// The ONE rule of
    /// what a cause is: a finding with no code makes none (this
    /// finding's sentence and place already state all it says — a
    /// refusal stated as a sentence; every rule of the manifest loader's
    /// own has a code), and a finding reported
    /// under its own code makes none (nothing is wrapped: the row IS
    /// the finding — NML2081, NML2082). A wrapped finding that is itself a
    /// wrapper contributes its own cause, the innermost, in that
    /// cause's file — its own, else the wrapped finding's, else
    /// `source` — never a chain.
    pub fn caused_by(mut self, finding: &Diagnostic, source: Option<String>) -> Self {
        // The remedies first, stamped explicitly: a front end may show
        // this row on a document that is not the file the edit lands in
        // (the editor's note on a governed file), and an unstamped edit
        // would resolve against that document's text.
        let carried: Vec<Suggestion> = finding
            .suggestions
            .iter()
            .map(|s| {
                let file = finding
                    .suggestion_source(s)
                    .map(str::to_owned)
                    .or_else(|| source.clone())
                    .or_else(|| self.source.clone());
                match file {
                    Some(file) => s.clone().in_file(file),
                    None => s.clone(),
                }
            })
            .collect();
        self.suggestions.extend(carried);
        let cause = match finding.cause.as_deref() {
            Some(inner) => Some(Cause {
                source: inner
                    .source
                    .clone()
                    .or_else(|| finding.source.clone())
                    .or(source),
                ..inner.clone()
            }),
            None => finding.code.map(|code| Cause {
                code,
                message: finding.message.clone(),
                span: finding.span,
                source,
            }),
        };
        self.cause = cause
            .filter(|cause| Some(cause.code) != self.code)
            .map(Box::new);
        self
    }

    /// The file the cause's span indexes into: its own, else this
    /// finding's — the inheritance a note has ([`Self::related_source`]).
    pub fn cause_source(&self) -> Option<&str> {
        self.inherit(self.cause.as_deref().and_then(|c| c.source.as_deref()))
    }

    /// The file `s`'s span indexes into: the suggestion's own, else the
    /// diagnostic's — the same inheritance a note has, so an applier
    /// routes an edit to its file by one rule.
    pub fn suggestion_source<'s>(&'s self, s: &'s Suggestion) -> Option<&'s str> {
        self.inherit(s.source.as_deref())
    }

    fn inherit<'s>(&'s self, own: Option<&'s str>) -> Option<&'s str> {
        own.or(self.source.as_deref())
    }

    /// Attach a machine-applicable edit — the ONE way a suggestion joins
    /// a finding. The suggestion itself is built by its kind:
    /// `Suggestion::did_you_mean("warn").at(span)`,
    /// `Suggestion::fix("slot as a").at(span)` (once per alternative),
    /// `Suggestion::delete().at(span)`,
    /// `Suggestion::insert(block).at(span).in_file(manifest)` — every
    /// fact named, no argument order to get wrong.
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestions.push(suggestion);
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

/// Where a producer PUSHES its findings (RFC 0019 item 0). A
/// `Vec<Diagnostic>` is a sink — the library's default: `validate`
/// returns one, and every consumer that wants the whole list keeps it —
/// and a front end may STREAM: the CLI's reporter tallies exactly and
/// prints within its budget as each finding is derived, so a document
/// yielding a million findings costs the run its printing budget, never
/// a million materialized findings; the editor keeps the first
/// [`Bounded`] tranche it publishes and counts the rest. Object-safe
/// (`&mut dyn DiagnosticSink`), so the validator's forty threaded
/// parameters carry no type parameter.
pub trait DiagnosticSink {
    fn push(&mut self, diag: Diagnostic);

    /// Push every finding of a scratch collection (a sub-validation
    /// admitted as a whole), in order.
    fn absorb(&mut self, diags: Vec<Diagnostic>) {
        for diag in diags {
            self.push(diag);
        }
    }
}

impl DiagnosticSink for Vec<Diagnostic> {
    fn push(&mut self, diag: Diagnostic) {
        Vec::push(self, diag);
    }

    fn absorb(&mut self, diags: Vec<Diagnostic>) {
        self.extend(diags);
    }
}

/// Single cap on reported diagnostics, applied *at emission* by the lexer, the
/// parser, lowering, value decoding, the source-character policy and the
/// entry-name rule, then again on each merged list — so memory stays bounded
/// during and after parsing on pathological input (RFC 0004 §9, "bounded
/// output").
///
/// It lives here, with the diagnostic, because it is a diagnostic budget and
/// not a parse fact: every pass that reports has one, and a pass the parser
/// CALLS must be able to read its own cap without naming the parser. (It sat
/// in `cst`, which `source_policy` and `entry_names` — both of them called
/// from `cst` — reached up for; `entry_names` and `cst` were a dependency
/// cycle whose only content was this line.)
///
/// LIMIT: reach=content guards=output surface=kernel shown="128" — parse/lex findings REPORTED per document; the rest are counted, never dropped silently
pub(crate) const MAX_ERRORS: usize = 128;

/// A sink that KEEPS the first findings up to `cap` entries of `out`
/// (whatever `out` already held counts) and COUNTS the rest, exactly —
/// a front end that publishes a bounded list holds no more than it
/// publishes, and its "N further finding(s) not shown" row is the truth
/// (the editor's `MAX_DIAGNOSTICS`, applied at the source).
pub struct Bounded<'a> {
    out: &'a mut Vec<Diagnostic>,
    cap: usize,
    /// The findings pushed past the cap.
    pub elided: usize,
}

impl<'a> Bounded<'a> {
    pub fn new(out: &'a mut Vec<Diagnostic>, cap: usize) -> Self {
        Self {
            out,
            cap,
            elided: 0,
        }
    }
}

impl DiagnosticSink for Bounded<'_> {
    fn push(&mut self, diag: Diagnostic) {
        if self.out.len() < self.cap {
            self.out.push(diag);
        } else {
            self.elided += 1;
        }
    }
}

/// A sink that forwards only the findings `keep` admits — a front end's
/// suppression applied AT THE SOURCE, ahead of a [`Bounded`] tranche, so
/// the tranche holds only findings that will be published and the count
/// past it never includes one the front end would have dropped.
pub struct Filtered<'a, S: ?Sized> {
    inner: &'a mut S,
    keep: &'a dyn Fn(&Diagnostic) -> bool,
}

impl<'a, S: DiagnosticSink + ?Sized> Filtered<'a, S> {
    pub fn new(inner: &'a mut S, keep: &'a dyn Fn(&Diagnostic) -> bool) -> Self {
        Self { inner, keep }
    }
}

impl<S: DiagnosticSink + ?Sized> DiagnosticSink for Filtered<'_, S> {
    fn push(&mut self, diag: Diagnostic) {
        if (self.keep)(&diag) {
            self.inner.push(diag);
        }
    }
}

/// A character every rendering surface escapes and no machine-applied
/// replacement may carry: every Unicode control (line breaks included —
/// new *structure* must never ride a fix) and the source policy's
/// banned raw set (`must_escape`: the controls again, plus CR, the
/// Trojan-Source bidi controls, interior U+FEFF, and the U+2028/U+2029
/// separators). The union is exactly `is_control ∪ must_escape` — the
/// guard set, the render-escape set, and the language's own raw-source
/// policy are ONE congruent fact, shared by the renderer's choke point,
/// the CLI's note lines, and the structural resolver's injection guard,
/// so no set can drift from the others.
pub fn needs_escape(ch: char) -> bool {
    ch.is_control() || crate::source_policy::must_escape(ch)
}

/// Write `text` with hostile characters escaped (`\n` → `\u{a}`-style,
/// [`needs_escape`]), still zero-alloc. Diagnostics echo *untrusted
/// source text* (found tokens, bad literals, enum values); a malicious
/// file must not be able to smuggle terminal escape sequences — or
/// invisible bidi steering — into CLI output or log lines. One choke
/// point — every render path goes through [`Rendered`], so no producer
/// has to remember.
fn write_sanitized(f: &mut fmt::Formatter<'_>, text: &str) -> fmt::Result {
    use fmt::Write as _;
    for ch in text.chars() {
        if needs_escape(ch) {
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
        // Structural kinds (`Delete`, `Insert`) render nothing here — the
        // producer's prose states the action, and an insertion's block is
        // a front end's own surface (the CLI's `help:` beneath the row,
        // the editor's quick-fix); the filters below select by kind, so
        // neither matches. The empty-replacement guard on did-you-means is
        // defense in depth (no producer emits one): `(did you mean ""?)`
        // reads as nonsense.
        let dym: Vec<&Suggestion> = self
            .0
            .suggestions
            .iter()
            .filter(|s| s.kind == SuggestionKind::DidYouMean && !s.replacement.is_empty())
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
                    // A deletion alternative says so, in the singular
                    // arm's vocabulary — backticks stay reserved for
                    // verbatim replacement text.
                    if s.replacement.is_empty() {
                        f.write_str("remove")?;
                    } else {
                        f.write_str("`")?;
                        write_sanitized(f, &s.replacement)?;
                        f.write_str("`")?;
                    }
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
    /// A fence's docs-test tags never reach a reader — every opening
    /// fence in every explain document is `\`\`\`<language>` alone, and the
    /// fenced text itself is untouched.
    #[test]
    fn explain_documents_carry_no_fence_tags() {
        let mut fences = 0usize;
        for (code, _) in super::explain_index() {
            let doc = super::explain_document(code).expect("every indexed code has a document");
            let mut in_fence = false;
            for line in doc.lines() {
                let Some(info) = line.trim_start().strip_prefix("```") else {
                    continue;
                };
                if !in_fence {
                    fences += 1;
                    assert!(
                        !info.contains(char::is_whitespace),
                        "{code}: an opening fence kept its tags: {line:?}"
                    );
                }
                in_fence = !in_fence;
            }
        }
        assert!(fences > 100, "the index has fenced examples: {fences}");
        let doc = super::explain_document("NML2087").expect("NML2087");
        assert!(
            doc.contains("```text\n$ nml check --root . shared/x.flow.nml"),
            "{doc}"
        );
    }

    use super::*;

    // Allocation invariants (uniqueness, band, ordering) are proven at
    // COMPILE time by the guard in `codes` — a runtime test could only
    // re-assert what already failed to build, so none exists.

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
    /// The wrapped finding's remedies ride the wrapper row, each in its
    /// file — the suggestion's own, else the wrapped finding's, else the
    /// `source` given, else the row's own — stamped explicitly, so a
    /// renderer that shows the row on another document still routes the
    /// edit to the file it lands in; the row's rendering carries the
    /// did-you-mean hint the finding carried. A codeless finding's
    /// remedy rides too (the cause rule is the code's, the remedy's is
    /// the edit's).
    #[test]
    fn caused_by_carries_the_wrapped_findings_remedies_in_their_file() {
        let span = Span::new(4, 10);
        let inner = Diagnostic::error("unknown property 'versio'")
            .with_code(codes::UNKNOWN_PROPERTY)
            .with_span(span)
            .with_suggestion(Suggestion::did_you_mean("version").at(span));
        let row = Diagnostic::error("manifest failed to load")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .with_source("demo.package.nml")
            .caused_by(&inner, None);
        assert_eq!(
            row.suggestions,
            vec![
                Suggestion::did_you_mean("version")
                    .at(span)
                    .in_file("demo.package.nml")
            ],
            "the row's own file, stamped"
        );
        assert_eq!(
            row.rendered_message(),
            "manifest failed to load (did you mean \"version\"?)"
        );
        let given = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .caused_by(&inner, Some("given.nml".to_string()));
        assert_eq!(given.suggestions[0].source.as_deref(), Some("given.nml"));
        let owned = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .caused_by(
                &inner.clone().with_source("own.nml"),
                Some("given.nml".to_string()),
            );
        assert_eq!(
            owned.suggestions[0].source.as_deref(),
            Some("own.nml"),
            "the wrapped finding's file wins over the one given"
        );
        let elsewhere = Diagnostic::error("x")
            .with_code(codes::UNKNOWN_PROPERTY)
            .with_span(span)
            .with_suggestion(Suggestion::insert("y = 1").at(span).in_file("other.nml"));
        let routed = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .with_source("demo.package.nml")
            .caused_by(&elsewhere, None);
        assert_eq!(
            routed.suggestions[0].source.as_deref(),
            Some("other.nml"),
            "a suggestion's own file wins over everything"
        );
        let nowhere = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .caused_by(&inner, None);
        assert_eq!(
            nowhere.suggestions[0].source, None,
            "no file known: unstamped"
        );
        // EVERY remedy rides, not the first: a finding offering N
        // alternatives (one `Fix` each) hands the row all of them, each
        // stamped with the file it edits.
        let two = Diagnostic::error("slot is ambiguous")
            .with_code(codes::UNKNOWN_PROPERTY)
            .with_span(span)
            .with_suggestion(Suggestion::fix("slot as a").at(span))
            .with_suggestion(Suggestion::fix("slot as b").at(span));
        let row = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .with_source("demo.package.nml")
            .caused_by(&two, None);
        assert_eq!(
            row.suggestions,
            vec![
                Suggestion::fix("slot as a")
                    .at(span)
                    .in_file("demo.package.nml"),
                Suggestion::fix("slot as b")
                    .at(span)
                    .in_file("demo.package.nml"),
            ],
            "every alternative rides, in order: {row:?}"
        );
        let codeless = Diagnostic::error("shape")
            .with_span(span)
            .with_suggestion(Suggestion::delete().at(span));
        let row = Diagnostic::error("row")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .with_source("m.nml")
            .caused_by(&codeless, None);
        assert!(row.cause.is_none() && row.suggestions.len() == 1, "{row:?}");
    }

    /// `caused_by` is the one rule of what a cause is: a coded finding
    /// wrapped under another code rides as the cause (its code, sentence
    /// and span; the file given, else the row's), a codeless finding
    /// makes none, a finding reported under its own code makes none, and
    /// a wrapped wrapper contributes its own cause — the innermost, in
    /// its own file — never a chain.
    #[test]
    fn caused_by_keeps_the_innermost_coded_finding_and_nothing_else() {
        use super::{Cause, Diagnostic, codes};
        use crate::span::Span;
        let inner = Diagnostic::error("dup")
            .with_code(codes::DUPLICATE_ENTRY)
            .with_span(Span::new(3, 7));
        let row = Diagnostic::error("wrap")
            .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
            .with_source("m.nml")
            .caused_by(&inner, None);
        assert_eq!(
            row.cause.as_deref(),
            Some(&Cause {
                code: codes::DUPLICATE_ENTRY,
                message: "dup".to_string(),
                span: Some(Span::new(3, 7)),
                source: None,
            })
        );
        assert_eq!(row.cause_source(), Some("m.nml"), "inherits the row's file");
        let codeless = Diagnostic::error("shape").with_span(Span::new(0, 1));
        assert!(
            Diagnostic::error("wrap")
                .with_code(codes::RESOLUTION_INPUT_UNLOADABLE)
                .caused_by(&codeless, None)
                .cause
                .is_none(),
            "a codeless finding hides no fact the row does not state"
        );
        let own = Diagnostic::error("grant").with_code(codes::LAYER_GRANT_RULE);
        assert!(
            Diagnostic::error("wrap")
                .with_code(codes::LAYER_GRANT_RULE)
                .caused_by(&own, None)
                .cause
                .is_none(),
            "reported under its own code: nothing is wrapped"
        );
        let outer = Diagnostic::error("outer")
            .with_code(codes::VALIDATOR_UNBUILDABLE)
            .caused_by(&row, Some("elsewhere.nml".to_string()));
        let cause = outer.cause.as_deref().expect("the innermost");
        assert_eq!(cause.code, codes::DUPLICATE_ENTRY, "never a chain");
        assert_eq!(
            cause.source.as_deref(),
            Some("m.nml"),
            "the innermost's file: its own, else the wrapped finding's, else the one given"
        );
    }

    #[test]
    fn rendered_message_renders_fix_alternatives_capped() {
        let mut d = Diagnostic::error("ambiguous").with_span(Span::new(0, 4));
        for v in ["a", "b", "c", "d"] {
            d = d.with_suggestion(Suggestion::fix(format!("slot as {v}")).at(Span::new(0, 4)));
        }
        let out = d.rendered_message();
        assert!(
            out.contains("(fixes: `slot as a`, `slot as b`, `slot as c`, … and 1 more)"),
            "{out}"
        );
        let single = Diagnostic::error("x")
            .with_suggestion(Suggestion::fix("slot as a").at(Span::new(0, 4)));
        assert!(single.rendered_message().contains("(fix: `slot as a`)"));
    }

    #[test]
    fn rendered_message_derives_hint_from_suggestion() {
        let diag = Diagnostic::error("invalid value \"wran\"")
            .with_suggestion(Suggestion::did_you_mean("warn").at(Span::new(1, 5)));
        assert_eq!(
            diag.rendered_message(),
            "invalid value \"wran\" (did you mean \"warn\"?)"
        );
        assert_eq!(
            diag.to_string(),
            "error: invalid value \"wran\" (did you mean \"warn\"?)"
        );
    }

    /// One door, every kind: the four named entries are `of`'s spellings and
    /// build the exact struct — the kind, the payload, the anchor, no file;
    /// `in_file` names the file; a deletion carries no text whatever the
    /// caller (or a wire) handed it.
    #[test]
    fn every_kind_builds_through_one_door_to_the_exact_struct() {
        let span = Span::new(3, 9);
        let cases = [
            (
                Suggestion::did_you_mean("warn"),
                SuggestionKind::DidYouMean,
                "warn",
            ),
            (
                Suggestion::fix("slot as a"),
                SuggestionKind::Fix,
                "slot as a",
            ),
            (Suggestion::delete(), SuggestionKind::Delete, ""),
            (
                Suggestion::insert("layers:\n    allowRefs:"),
                SuggestionKind::Insert,
                "layers:\n    allowRefs:",
            ),
        ];
        for (built, kind, replacement) in cases {
            let want = Suggestion {
                replacement: replacement.to_string(),
                span,
                kind,
                source: None,
            };
            assert_eq!(built.clone().at(span), want, "{kind:?}");
            assert_eq!(
                Suggestion::of(kind, replacement).at(span),
                want,
                "{kind:?} via `of`"
            );
            assert_eq!(
                built.at(span).in_file("m.nml"),
                Suggestion {
                    source: Some("m.nml".to_string()),
                    ..want
                },
                "{kind:?} in a file"
            );
        }
        assert_eq!(
            Suggestion::of(SuggestionKind::Delete, "not a payload").at(span),
            Suggestion::delete().at(span),
            "a deletion carries no text whatever it was handed"
        );
    }

    #[test]
    fn a_deletion_renders_nothing_and_stays_structural() {
        // `Delete` is structural (NML2060's "delete this assignment"):
        // the producer's prose states the action, so the renderer adds no
        // hint — and the suggestion survives for the resolver.
        let diag = Diagnostic::error("'x' is sealed — delete this assignment")
            .with_suggestion(Suggestion::delete().at(Span::new(1, 5)));
        assert_eq!(
            diag.rendered_message(),
            "'x' is sealed — delete this assignment"
        );
        assert_eq!(diag.suggestions.len(), 1, "the machine fix survives");
        assert_eq!(diag.suggestions[0].kind, SuggestionKind::Delete);
        // Defense in depth: an empty DID-YOU-MEAN (no producer emits one)
        // still renders no `(did you mean ""?)` nonsense.
        let legacy = Diagnostic::error("x")
            .with_suggestion(Suggestion::did_you_mean("").at(Span::new(1, 5)));
        assert_eq!(legacy.rendered_message(), "x");
    }

    /// An insertion is structural like a deletion: the message renders
    /// no hint (its block is a front end's own surface), and the edit
    /// knows its file by the inheritance a note has — its own `source`,
    /// else the diagnostic's.
    #[test]
    fn an_insertion_renders_nothing_and_knows_its_file() {
        let own = Diagnostic::error("denied")
            .with_source("a.nml")
            .with_suggestion(Suggestion::insert("y = 1").at(Span::new(1, 5)));
        assert_eq!(own.rendered_message(), "denied");
        let [s] = own.suggestions.as_slice() else {
            panic!("{:?}", own.suggestions);
        };
        assert_eq!(s.kind, SuggestionKind::Insert);
        assert_eq!(s.replacement, "y = 1");
        assert_eq!(own.suggestion_source(s), Some("a.nml"));
        let foreign = Diagnostic::error("denied")
            .with_source("a.nml")
            .with_suggestion(
                Suggestion::insert("y = 1")
                    .at(Span::new(1, 5))
                    .in_file("m.nml".to_string()),
            );
        assert_eq!(
            foreign.suggestion_source(&foreign.suggestions[0]),
            Some("m.nml")
        );
        assert_eq!(
            Diagnostic::error("x")
                .with_suggestion(Suggestion::insert("y = 1").at(Span::new(1, 5)))
                .suggestion_source(&Suggestion {
                    replacement: String::new(),
                    span: Span::new(0, 0),
                    kind: SuggestionKind::Insert,
                    source: None,
                }),
            None
        );
    }

    /// A deletion among ALTERNATIVES renders `remove` in the plural
    /// arm, exactly as the singular arm renders it — backticks stay
    /// reserved for verbatim replacement text, so an empty backtick
    /// pair can never appear.
    #[test]
    fn a_deletion_alternative_renders_remove_in_the_plural_arm() {
        let d = Diagnostic::error("x")
            .with_suggestion(Suggestion::fix("").at(Span::new(0, 1)))
            .with_suggestion(Suggestion::fix("\\u{202E}").at(Span::new(0, 1)));
        assert_eq!(d.rendered_message(), "x (fixes: remove, `\\u{202E}`)");
    }

    #[test]
    fn wire_names_round_trip_for_every_kind() {
        for kind in [
            SuggestionKind::DidYouMean,
            SuggestionKind::Fix,
            SuggestionKind::Delete,
            SuggestionKind::Insert,
        ] {
            assert_eq!(SuggestionKind::from_wire_name(kind.wire_name()), Some(kind));
        }
        assert_eq!(SuggestionKind::from_wire_name("nonsense"), None);
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

    /// The headline is the bold lead, cut clean; a summary with none
    /// headlines with itself. Every code's section opens with a bold
    /// lead that fits a terminal line — the shape `--list` and the
    /// palette rely on, held here.
    #[test]
    fn explain_headline_is_the_bold_lead_of_every_code() {
        assert_eq!(
            headline_of("**Missing required field.** A required field is absent."),
            "Missing required field."
        );
        assert_eq!(headline_of("no bold lead here"), "no bold lead here");
        assert!(explain_headline("NML9999").is_none());
        for (_, code) in codes::ALL {
            let code = code.to_string();
            let headline =
                explain_headline(&code).unwrap_or_else(|| panic!("{code} has no headline"));
            let summary = explain_summary(&code).unwrap_or_default();
            assert!(
                summary.starts_with("**") && headline != summary,
                "{code}: the section must open with a bold lead: {summary}"
            );
            // `nml explain --list` prints `NML0000  <headline>`: the code,
            // two spaces and the headline share the 80-column line every
            // page wraps at, so the bound is on the printed LINE.
            let line = format!("{code}  {headline}");
            assert!(
                !headline.is_empty() && !headline.contains("**") && line.chars().count() <= 80,
                "{code}: `{line}` is {} columns",
                line.chars().count()
            );
        }
    }

    #[test]
    fn strip_relative_links_never_rewrites_inline_code() {
        let text = "a `[](a | b)` element and a [doc](spec/x.md) link";
        assert_eq!(
            strip_relative_links(text),
            "a `[](a | b)` element and a doc link"
        );
    }

    #[test]
    fn strip_relative_links_unmatched_backtick_leaves_the_tail_verbatim() {
        // An unmatched backtick flips parity for the rest of the text:
        // everything after it is treated as code and passes through
        // verbatim — never mangled, never crashed.
        assert_eq!(strip_relative_links("a ` b [x](y)"), "a ` b [x](y)");
        assert_eq!(strip_relative_links("[x](y) ` tail"), "x ` tail");
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
        // Strictly ascending — the documented API guarantee. One assertion
        // for two properties: ordering (the palette/`--list` contract) and
        // uniqueness (strictly increasing implies distinct), the same
        // subsumption the compile-time allocation guard uses. `dedup` alone
        // caught only *adjacent* repeats, so this strengthens the old check
        // rather than restating it.
        let heads: Vec<&str> = index.iter().map(|(h, _)| *h).collect();
        assert!(
            heads.windows(2).all(|w| w[0] < w[1]),
            "index order must be strictly ascending (docs-test enforces the \
             same rule on the file): {heads:?}"
        );
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

        // The choke point speaks the FULL `needs_escape` set — the
        // Trojan-Source bidi controls and the U+2028/U+2029 separators
        // render escaped, exactly like the controls.
        let steering = Diagnostic::error("found \u{202E}x\u{2028}y");
        let out = steering.rendered_message();
        assert!(
            !out.contains('\u{202E}') && !out.contains('\u{2028}'),
            "{out}"
        );
        assert!(out.contains("\\u{202e}"), "{out}");
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
