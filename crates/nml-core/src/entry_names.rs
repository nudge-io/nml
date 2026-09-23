//! The kernel's one rule over NAMES: a scope declares each name once.
//!
//! Two scopes, two codes, one pass beside every parse
//! ([`crate::cst::parse_lowered`], the single home of the text → tree
//! pipeline, so no consumer can skip it — `parse_to_ast` refuses the
//! text as it refuses a stray token, the all-findings forms report it
//! located beside the syntax findings, and a best-effort tree keeps both
//! occurrences for structure-driven tooling):
//!
//! * **NML1000, the file scope** — every top-level declaration (block,
//!   array, `const`, `template`, `oneof`) shares one namespace so
//!   references stay unambiguous; a second declaration under one name is
//!   an error at the later name, the first a related note. A manifest's
//!   `package` block and `[]` arrays are declarations too.
//! * **NML2093, a body** — a second entry with the same name in ONE body
//!   (`version = "1"` twice, or a block `files:` beside an inline array
//!   `files = […]` — two spellings of one entry) is an error at the LATER
//!   occurrence, the first a related note. Which of the two is meant is
//!   unknowable, so no suggestion rides the row. TOML forbids the repeat
//!   outright and the YAML specification forbids it; here every reader
//!   that picked a winner (a lookup that stopped at the first, a loop
//!   whose last assignment stood) disagreed with its neighbour, and a
//!   manifest's closed-vocabulary guarantee rests on one `files` entry
//!   meaning what it says.
//!
//! "The same name" is exact bytes (`Files` is not `files`); in a body the
//! sigils are namespaces — a modifier `|allow` and a shared default
//! `.timeout` are not the entries `allow` and `timeout`; a modifier's type
//! declaration beside its value (`|deny []string`, then `|deny = […]`) is
//! declare-then-assign (RFC 0019 errata E12) — two entries, one name,
//! allowed; two declarations or two values are the duplicate. A property
//! (`k = v`), a nested block (`k:`) and a field definition (`k type`) are
//! one name. List items (`- x`) are positional, never named entries of
//! the body, and may repeat — a set's uniqueness is NML2030's — and arms
//! are keyed by selector (NML2036). Every body reachable from a
//! declaration is judged: nested blocks, the bodies of list items and
//! inline arm targets, `.shared:` blocks and `|modifier:` items. One map
//! per scope, keyed by name: linear in the entry count. The findings are
//! bounded at the parse's own cap with exact suppression accounting.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::ast::{
    ArmTarget, ArrayBody, Body, BodyEntryKind, DeclarationKind, File, Identifier, ListItem,
    ListItemKind, Modifier, ModifierValue, SharedPropertyKind,
};
use crate::error::{NmlError, ParseErrorKind};
use crate::span::Span;

/// The namespace an entry's name lives in: the sigil is part of the
/// identity, so `|allow` and `allow` are two names.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Namespace {
    /// `k = v`, `k:`, `k type` — the plain name.
    Plain,
    /// `|k = …`, `|k:` — an access-control modifier's value.
    Modifier,
    /// `|k type` — a modifier's type declaration (RFC 0019 errata E12):
    /// beside its value it is declare-then-assign, not a repeat.
    ModifierDeclaration,
    /// `.k` — a shared default for the body's list items.
    Shared,
}

/// How an entry spelled its name — for the clarifier when the two
/// spellings of one entry meet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    /// `k = v`
    Value,
    /// `k:` with a body
    Block,
    /// `k type`, `|k`, `.k`
    Other,
}

/// The names one body has declared so far: the first occurrence of each.
type Seen<'a> = HashMap<(Namespace, &'a str), (Span, Spelling)>;

/// The bounded findings of one pass: past the parse's cap the rest are
/// counted, never dropped silently (RFC 0009 exact-count honesty).
struct Sink {
    errors: Vec<NmlError>,
    suppressed: usize,
}

impl Sink {
    fn push(&mut self, kind: ParseErrorKind, span: Span) {
        if self.errors.len() < crate::diagnostic::MAX_ERRORS {
            self.errors.push(NmlError::Syntax { kind, span });
        } else {
            self.suppressed += 1;
        }
    }
}

/// The name rules over `file`: the file scope, then every body of its
/// declarations in document order — `(bounded errors, suppressed count)`,
/// the shape [`crate::source_policy::check`] returns beside it.
pub(crate) fn check(file: &File) -> (Vec<NmlError>, usize) {
    let mut sink = Sink {
        errors: Vec::new(),
        suppressed: 0,
    };
    declarations(file, &mut sink);
    for decl in &file.declarations {
        match &decl.kind {
            DeclarationKind::Block(block) => body(&block.body, &mut sink),
            DeclarationKind::Array(array) => array_body(&array.body, &mut sink),
            // A constant and a template hold one value; a `oneof` holds
            // arms keyed by discriminator value (NML2015).
            DeclarationKind::Const(_)
            | DeclarationKind::Template(_)
            | DeclarationKind::OneOf(_) => {}
        }
    }
    (sink.errors, sink.suppressed)
}

/// The file scope (NML1000): one namespace across every declaration
/// kind, keyed by the bare name — the identity references resolve by.
fn declarations(file: &File, sink: &mut Sink) {
    let mut seen: HashMap<&str, Span> = HashMap::new();
    for decl in &file.declarations {
        let name: &Identifier = match &decl.kind {
            DeclarationKind::Block(b) => &b.name,
            DeclarationKind::Array(a) => &a.name,
            DeclarationKind::Const(c) => &c.name,
            DeclarationKind::Template(t) => &t.name,
            DeclarationKind::OneOf(o) => &o.name,
        };
        // A declaration the parser could not name lowers with an empty name
        // (the parse finding already marks the site): it declares nothing
        // and collides with nothing.
        if name.name.is_empty() {
            continue;
        }
        match seen.entry(name.name.as_str()) {
            Entry::Occupied(first) => sink.push(
                ParseErrorKind::DuplicateDeclaration {
                    name: name.name.clone(),
                    first: *first.get(),
                },
                name.span,
            ),
            Entry::Vacant(slot) => {
                slot.insert(name.span);
            }
        }
    }
}

/// One body and every body beneath it.
fn body(body: &Body, sink: &mut Sink) {
    let mut seen = Seen::new();
    for entry in &body.entries {
        match &entry.kind {
            BodyEntryKind::Property(p) => {
                see(&mut seen, Namespace::Plain, &p.name, Spelling::Value, sink)
            }
            BodyEntryKind::NestedBlock(nb) => {
                see(&mut seen, Namespace::Plain, &nb.name, Spelling::Block, sink);
                self::body(&nb.body, sink);
            }
            BodyEntryKind::FieldDefinition(f) => {
                see(&mut seen, Namespace::Plain, &f.name, Spelling::Other, sink);
            }
            BodyEntryKind::Modifier(m) => {
                see(
                    &mut seen,
                    modifier_namespace(m),
                    &m.name,
                    Spelling::Other,
                    sink,
                );
                if let ModifierValue::Block(items) = &m.value {
                    for item in items {
                        item_body(item, sink);
                    }
                }
            }
            BodyEntryKind::SharedProperty(sp) => {
                see(
                    &mut seen,
                    Namespace::Shared,
                    &sp.name,
                    Spelling::Other,
                    sink,
                );
                if let SharedPropertyKind::Block(block) = &sp.kind {
                    self::body(block, sink);
                }
            }
            BodyEntryKind::ListItem(item) => item_body(item, sink),
            BodyEntryKind::Arm(arm) => {
                if let ArmTarget::Inline { body, .. } = &arm.target {
                    self::body(body, sink);
                }
            }
        }
    }
}

/// An array declaration's body: its own entries — modifiers, shared
/// defaults, properties — then every item's body.
fn array_body(body: &ArrayBody, sink: &mut Sink) {
    let mut seen = Seen::new();
    for m in &body.modifiers {
        see(
            &mut seen,
            modifier_namespace(m),
            &m.name,
            Spelling::Other,
            sink,
        );
        if let ModifierValue::Block(items) = &m.value {
            for item in items {
                item_body(item, sink);
            }
        }
    }
    for sp in &body.shared_properties {
        see(
            &mut seen,
            Namespace::Shared,
            &sp.name,
            Spelling::Other,
            sink,
        );
        if let SharedPropertyKind::Block(block) = &sp.kind {
            self::body(block, sink);
        }
    }
    for p in &body.properties {
        see(&mut seen, Namespace::Plain, &p.name, Spelling::Value, sink);
    }
    for item in &body.items {
        item_body(item, sink);
    }
}

/// A modifier's type declaration (`|k type`) and its value (`|k = …`,
/// `|k:`) are declare-then-assign — two entries, one name, as authored
/// (RFC 0019 errata E12) — so each lives in its own namespace: two
/// declarations, or two values, are the duplicate.
fn modifier_namespace(m: &Modifier) -> Namespace {
    match m.value {
        ModifierValue::TypeAnnotation { .. } => Namespace::ModifierDeclaration,
        ModifierValue::Inline(_) | ModifierValue::Block(_) => Namespace::Modifier,
    }
}

/// A list item's body, when it has one — a named item's, or the block a
/// scalar shorthand carries. The item itself is positional: never a name.
fn item_body(item: &ListItem, sink: &mut Sink) {
    match &item.kind {
        ListItemKind::Named { body, .. } => self::body(body, sink),
        ListItemKind::Shorthand {
            body: Some(body), ..
        } => self::body(body, sink),
        ListItemKind::Shorthand { body: None, .. }
        | ListItemKind::Reference(_)
        | ListItemKind::Role(_) => {}
    }
}

/// Record `name` as seen, or report it against the first so named: at
/// the later name, the first the note; the clarifier only when the two
/// spellings of one entry met.
fn see<'a>(
    seen: &mut Seen<'a>,
    ns: Namespace,
    name: &'a Identifier,
    spelling: Spelling,
    sink: &mut Sink,
) {
    // An entry the parser could not name (see `declarations`) is no entry.
    if name.name.is_empty() {
        return;
    }
    match seen.entry((ns, name.name.as_str())) {
        Entry::Occupied(first) => {
            let (first_span, first_spelling) = *first.get();
            let shown = match ns {
                Namespace::Plain => name.name.clone(),
                Namespace::Modifier | Namespace::ModifierDeclaration => format!("|{}", name.name),
                Namespace::Shared => format!(".{}", name.name),
            };
            let two_spellings = matches!(
                (first_spelling, spelling),
                (Spelling::Block, Spelling::Value) | (Spelling::Value, Spelling::Block)
            );
            sink.push(
                ParseErrorKind::DuplicateEntry {
                    shown,
                    first: first_span,
                    two_spellings,
                },
                name.span,
            );
        }
        Entry::Vacant(slot) => {
            slot.insert((name.span, spelling));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BodyEntry, Property};
    use crate::diagnostic::{Diagnostic, Severity, codes};
    use crate::types::{SpannedValue, Value};

    /// The module's own findings over a best-effort tree, as diagnostics.
    fn findings(src: &str) -> Vec<Diagnostic> {
        let file = crate::cst::parse_best_effort(src);
        let (errors, suppressed) = check(&file);
        assert_eq!(suppressed, 0);
        errors.iter().map(NmlError::to_diagnostic).collect()
    }

    /// The byte offset of the `n`th (0-based) occurrence of `needle`.
    fn nth(src: &str, needle: &str, n: usize) -> usize {
        src.match_indices(needle).nth(n).map(|(i, _)| i).unwrap()
    }

    /// A stray token where a declaration starts (`\n` typed as two
    /// characters, a pasted line) recovers as a declaration with no name;
    /// two of them are not `duplicate declaration ''` at 1:1 ahead of the
    /// parse finding that names the stray token — they are nothing.
    #[test]
    fn a_declaration_the_parser_could_not_name_is_no_duplicate() {
        let src = "thing a:\n    v = \"x\"\n\\n[]b c:\\n[]d e:\n";
        let file = crate::cst::parse_best_effort(src);
        let nameless = file
            .declarations
            .iter()
            .filter(|d| match &d.kind {
                DeclarationKind::Block(b) => b.name.name.is_empty(),
                DeclarationKind::Array(a) => a.name.name.is_empty(),
                _ => false,
            })
            .count();
        assert!(
            nameless >= 2,
            "the text recovers {nameless} nameless declaration(s): {file:?}"
        );
        let out = findings(src);
        assert!(out.is_empty(), "{out:?}");
        // The same rule inside a body. No text this parser recovers reaches it
        // (a body entry always carries its name token; the empty name is the
        // AST's `Option` fallback), so the branch is pinned over the shape
        // directly: two entries with the empty name are two nothings, while
        // two entries sharing a real name are still one NML2093.
        let nameless = Body::fresh(
            (0..2)
                .map(|i| BodyEntry {
                    kind: BodyEntryKind::Property(Property {
                        name: Identifier::new(String::new(), Span::new(i, i + 1)),
                        value: SpannedValue::new(Value::Bool(true), Span::new(i, i + 1)),
                    }),
                    span: Span::new(i, i + 1),
                })
                .collect(),
        );
        let mut sink = Sink {
            errors: Vec::new(),
            suppressed: 0,
        };
        body(&nameless, &mut sink);
        assert!(sink.errors.is_empty(), "{:?}", sink.errors);
        let named = Body::fresh(
            (0..2)
                .map(|i| BodyEntry {
                    kind: BodyEntryKind::Property(Property {
                        name: Identifier::new("v".to_string(), Span::new(i, i + 1)),
                        value: SpannedValue::new(Value::Bool(true), Span::new(i, i + 1)),
                    }),
                    span: Span::new(i, i + 1),
                })
                .collect(),
        );
        let mut sink = Sink {
            errors: Vec::new(),
            suppressed: 0,
        };
        body(&named, &mut sink);
        assert_eq!(sink.errors.len(), 1, "{:?}", sink.errors);
    }

    #[test]
    fn a_property_twice_is_an_error_at_the_later_with_the_first_noted() {
        let src = "thing t:\n    v = \"x\"\n    v = \"y\"\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        let d = &out[0];
        assert_eq!(d.code, Some(codes::DUPLICATE_ENTRY));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(
            d.message,
            "duplicate entry 'v' — a body declares each name once"
        );
        let second = nth(src, "v = ", 1);
        assert_eq!(d.span, Some(Span::new(second, second + 1)));
        assert_eq!(d.related.len(), 1);
        let first = nth(src, "v = ", 0);
        assert_eq!(d.related[0].span, Span::new(first, first + 1));
        assert_eq!(d.related[0].message, "'v' first declared here");
        assert_eq!(d.related[0].source, None, "same file: the note inherits");
        assert!(
            d.suggestions.is_empty(),
            "which entry is meant is unknowable — no suggestion, so no remedy line"
        );
    }

    #[test]
    fn a_block_beside_an_inline_array_is_one_entry_in_two_spellings() {
        let src = "thing t:\n    files:\n        - \"a\"\n    files = [\"b\"]\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry 'files' — a body declares each name once (`files:` and `files = …` \
             are two spellings of one entry)"
        );
        let inline = nth(src, "files = ", 0);
        assert_eq!(out[0].span, Some(Span::new(inline, inline + 5)));
        let block = nth(src, "files:", 0);
        assert_eq!(out[0].related[0].span, Span::new(block, block + 5));
        // The other order names the same pair.
        let src = "thing t:\n    files = [\"b\"]\n    files:\n        - \"a\"\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            out[0].message.ends_with("are two spellings of one entry)"),
            "{}",
            out[0].message
        );
        let block = nth(src, "files:", 0);
        assert_eq!(out[0].span, Some(Span::new(block, block + 5)));
    }

    #[test]
    fn a_third_occurrence_notes_the_first_not_the_second() {
        let src = "thing t:\n    v = 1\n    v = 2\n    v = 3\n";
        let out = findings(src);
        assert_eq!(out.len(), 2, "{out:?}");
        let first = nth(src, "v = ", 0);
        for d in &out {
            assert_eq!(d.related[0].span, Span::new(first, first + 1));
        }
        assert_eq!(out[0].span.unwrap().start, nth(src, "v = ", 1));
        assert_eq!(out[1].span.unwrap().start, nth(src, "v = ", 2));
    }

    #[test]
    fn sigils_are_namespaces_and_bytes_are_exact() {
        // A modifier's type declaration beside its value is declare-then-
        // assign (RFC 0019 errata E12): two entries, one name, allowed…
        let src = "thing t:\n    |deny []string\n    |deny = [\"a\"]\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
        // …two declarations, or two values, are the duplicate.
        let src = "thing t:\n    |deny []string\n    |deny []string\n";
        assert_eq!(findings(src).len(), 1, "{:?}", findings(src));
        // `allow`, `|allow`, `.allow` and `Allow` are four names.
        let src =
            "thing t:\n    allow = 1\n    |allow = [@public]\n    .allow = 2\n    Allow = 3\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
        // …and each namespace has its own rule.
        let src = "thing t:\n    |allow = [@public]\n    |allow = [@admin]\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry '|allow' — a body declares each name once"
        );
        assert_eq!(out[0].related[0].message, "'|allow' first declared here");
        let src = "thing t:\n    .retry = 1\n    .retry = 2\n    - a:\n        v = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry '.retry' — a body declares each name once"
        );
    }

    #[test]
    fn a_field_defined_twice_in_a_model_body_is_the_same_rule() {
        let src = "model m:\n    a string\n    a number\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry 'a' — a body declares each name once"
        );
        assert_eq!(out[0].span.unwrap().start, nth(src, "a number", 0));
        assert_eq!(out[0].related[0].span.start, nth(src, "a string", 0));
    }

    #[test]
    fn list_items_are_positional_and_never_flagged_but_their_bodies_are_judged() {
        let src = "thing t:\n    items:\n        - a:\n            v = 1\n        - a:\n            v = 2\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
        let src = "thing t:\n    items:\n        - a:\n            v = 1\n        - b:\n            v = 1\n            v = 2\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].span.unwrap().start, nth(src, "v = 2", 0));
        // A scalar shorthand's block, an inline arm target, a `.shared:`
        // block: every body beneath a declaration.
        let src =
            "thing t:\n    items:\n        - \"/api\":\n            v = 1\n            v = 2\n";
        assert_eq!(findings(src).len(), 1, "{:?}", findings(src));
        let src =
            "thing t:\n    landing:\n        else -> Page:\n            v = 1\n            v = 2\n";
        assert_eq!(findings(src).len(), 1, "{:?}", findings(src));
        let src = "thing t:\n    steps:\n        .retry:\n            n = 1\n            n = 2\n        - a:\n            v = 1\n";
        assert_eq!(findings(src).len(), 1, "{:?}", findings(src));
        // …and the items a `|modifier:` block holds.
        let src = "thing t:\n    |deny:\n        - a:\n            v = 1\n            v = 2\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].span.unwrap().start, nth(src, "v = 2", 0));
    }

    #[test]
    fn an_array_declaration_body_and_its_items_are_judged() {
        let src = "[]thing things:\n    x = 1\n    x = 2\n    - a:\n        v = 1\n        v = 2\n";
        let out = findings(src);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].span.unwrap().start, nth(src, "x = 2", 0));
        assert_eq!(out[1].span.unwrap().start, nth(src, "v = 2", 0));
    }

    /// An array declaration's own body is a scope like any other: beside
    /// its properties and items, its modifiers and its shared defaults are
    /// judged — each in its own namespace — and so is every body beneath
    /// them (a shared default's block, a `|modifier:` block's items).
    #[test]
    fn an_array_declarations_modifiers_and_shared_defaults_are_judged() {
        let src = "[]thing things:\n    |allow = [@public]\n    |allow = [@admin]\n    - x:\n        q = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry '|allow' — a body declares each name once"
        );
        assert_eq!(out[0].span.unwrap().start, nth(src, "allow", 1));

        let src = "[]thing things:\n    .retry = 1\n    .retry = 2\n    - x:\n        q = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(
            out[0].message,
            "duplicate entry '.retry' — a body declares each name once"
        );
        assert_eq!(out[0].span.unwrap().start, nth(src, "retry", 1));

        let src =
            "[]thing things:\n    .retry:\n        n = 1\n        n = 2\n    - x:\n        q = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].span.unwrap().start, nth(src, "n = 2", 0));

        let src = "[]thing things:\n    |deny:\n        - a:\n            v = 1\n            v = 2\n    - x:\n        q = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].span.unwrap().start, nth(src, "v = 2", 0));

        // The namespaces hold here too: a modifier, a shared default and a
        // property may share one name.
        let src = "[]thing things:\n    |retry = [@public]\n    .retry = 1\n    retry = 2\n    - x:\n        q = 1\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
    }

    #[test]
    fn a_body_with_distinct_names_is_silent() {
        let src = "thing t:\n    a = 1\n    b:\n        a = 2\n    c = [\"a\"]\n\nconst K = 1\n\ntemplate T:\n    \"x\"\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
    }

    /// The file scope (NML1000): the later declaration's NAME is the row,
    /// the first the note, the message in the body rule's voice.
    #[test]
    fn a_declaration_twice_is_an_error_at_the_later_name_with_the_first_noted() {
        let src = "service Api:\n    port = 8080\n\nservice Api:\n    port = 9090\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        let d = &out[0];
        assert_eq!(d.code, Some(codes::DUPLICATE_DECLARATION));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(
            d.message,
            "duplicate declaration 'Api' — a file declares each name once"
        );
        let second = nth(src, "Api", 1);
        assert_eq!(d.span, Some(Span::new(second, second + 3)));
        let first = nth(src, "Api", 0);
        assert_eq!(d.related[0].span, Span::new(first, first + 3));
        assert_eq!(d.related[0].message, "'Api' first declared here");
        assert!(d.suggestions.is_empty());
    }

    /// One namespace across every declaration kind — the identity
    /// references resolve by, keyword-blind (`model api` beside
    /// `service api`, a `const` beside a block, a `oneof` beside a model).
    #[test]
    fn every_declaration_kind_shares_the_file_namespace() {
        for src in [
            "model api:\n    port number\n\nservice api:\n    port = 1\n",
            "const K = 1\n\nthing K:\n    v = 1\n",
            "template K:\n    \"x\"\n\nconst K = 1\n",
            "model x:\n    a string\n\noneof x by k:\n    \"a\" -> x\n",
            "[]step steps:\n    - a\n\n[]step steps:\n    - b\n",
        ] {
            let out = findings(src);
            assert_eq!(
                out.iter()
                    .filter(|d| d.code == Some(codes::DUPLICATE_DECLARATION))
                    .count(),
                1,
                "{src:?}: {out:?}"
            );
        }
        // Distinct names never collide, whatever the keywords.
        let src = "model api:\n    port number\n\napi main:\n    port = 1\n\nconst K = 1\n";
        assert!(findings(src).is_empty(), "{:?}", findings(src));
    }

    /// A manifest's `package` block and `[]` arrays are declarations: a
    /// second one under the same name is the file-scope rule — refused
    /// where the text is parsed, before any loader reads a slot.
    #[test]
    fn a_manifests_second_package_block_or_array_under_one_name_is_a_duplicate_declaration() {
        let src = "package demo:\n    version = \"1\"\n    formatVersion = 1\n\npackage demo:\n    version = \"2\"\n    formatVersion = 1\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].code, Some(codes::DUPLICATE_DECLARATION));
        assert_eq!(out[0].span.map(|s| s.start), Some(nth(src, "demo", 1)));
        let src = "[]validator validators:\n    - a:\n        files:\n            - \"a.nml\"\n\n[]validator validators:\n    - b:\n        files:\n            - \"b.nml\"\n";
        let out = findings(src);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].code, Some(codes::DUPLICATE_DECLARATION));
        assert_eq!(
            out[0].span.map(|s| s.start),
            Some(nth(src, "validators", 1))
        );
    }

    /// Past the parse's cap the rest are counted, never dropped silently.
    #[test]
    fn findings_are_bounded_with_exact_suppression() {
        let mut src = String::from("thing t:\n");
        for _ in 0..(crate::diagnostic::MAX_ERRORS + 10) {
            src.push_str("    v = 1\n");
        }
        let file = crate::cst::parse_best_effort(&src);
        let (errors, suppressed) = check(&file);
        assert_eq!(errors.len(), crate::diagnostic::MAX_ERRORS);
        assert_eq!(suppressed, 9);
    }

    fn body_of(n: usize) -> Body {
        let entries = (0..n)
            .map(|i| BodyEntry {
                kind: BodyEntryKind::Property(Property {
                    name: Identifier::new(format!("p{i}"), Span::new(i, i + 1)),
                    value: SpannedValue::new(Value::Bool(true), Span::new(i, i + 1)),
                }),
                span: Span::new(i, i + 1),
            })
            .collect();
        Body::fresh(entries)
    }

    /// The walk is one map lookup per entry: a 100× larger body costs
    /// on the order of 100× (a quadratic walk would cost ~10 000×). A
    /// load-independent ratio, release only.
    #[test]
    #[ignore = "perf tier: cargo test -p nml-core --release --lib -- --ignored perf_"]
    fn perf_entry_names_walk_is_linear() {
        let small = body_of(2_000);
        let large = body_of(200_000);
        let time = |b: &Body| {
            let mut sink = Sink {
                errors: Vec::new(),
                suppressed: 0,
            };
            let start = std::time::Instant::now();
            for _ in 0..5 {
                body(b, &mut sink);
            }
            assert!(sink.errors.is_empty());
            start.elapsed()
        };
        let a = time(&small);
        let b = time(&large);
        let ratio = b.as_nanos() as f64 / a.as_nanos().max(1) as f64;
        assert!(
            ratio < 1_000.0,
            "200k/2k entries took {ratio:.0}× ({a:?} vs {b:?})"
        );
    }
}
