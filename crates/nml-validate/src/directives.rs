//! The directive vocabulary a schema source is judged under (RFC 0019 §Merge
//! policy, RFC 0030): the language's four merge-policy directives, EXTENDED
//! by the covering package's declared `[]directive` entries — one set and one
//! judge, so `nml check`, `nml validate` and the editor report the same rows
//! with the same sentences.

use nml_core::diagnostic::{Diagnostic, Suggestion, codes};
use nml_core::layers::BUILTIN_DIRECTIVES;
use nml_core::model::ModelDef;
use nml_core::span::Span;
use nml_core::types::Directive;

use crate::package::{DirectiveArg, DirectiveDecl};

/// One directive a vocabulary knows: a language builtin or a declared entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub arg: DirectiveArg,
    pub doc: &'a str,
    /// A language merge-policy directive (RFC 0019) rather than a declared
    /// entry.
    pub builtin: bool,
}

/// The directive vocabulary of one covering package: the builtins, then the
/// declared entries. A declared name never shadows a builtin — the manifest
/// loader refuses the redeclaration (NML2082) — and the builtins are
/// consulted first regardless, fail-closed.
#[derive(Debug, Clone)]
pub struct Vocabulary {
    package_name: String,
    declared: Vec<DirectiveDecl>,
}

impl Vocabulary {
    pub fn new(package_name: impl Into<String>, declared: Vec<DirectiveDecl>) -> Self {
        Self {
            package_name: package_name.into(),
            declared,
        }
    }

    /// The covering package's name — the one the unknown-name sentence
    /// cites.
    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    /// The package's own `[]directive` entries, in declaration order.
    pub fn declared(&self) -> &[DirectiveDecl] {
        &self.declared
    }

    /// Every entry: the language's builtins first, then the declared entries
    /// in declaration order — the completion menu's order.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> {
        BUILTIN_DIRECTIVES
            .iter()
            .map(|b| Entry {
                name: b.name,
                arg: DirectiveArg::None,
                doc: b.doc,
                builtin: true,
            })
            .chain(self.declared.iter().map(|d| Entry {
                name: &d.name,
                arg: d.arg,
                doc: &d.doc,
                builtin: false,
            }))
    }

    /// The entry named `name`, builtins first.
    pub fn get(&self, name: &str) -> Option<Entry<'_>> {
        self.entries().find(|e| e.name == name)
    }

    /// Judge every directive of every field of `models` (a source's own
    /// extraction, BEFORE inheritance resolution copies a trait's directives
    /// into its children): NML5000 for a name the vocabulary does not know,
    /// with a sigil-inclusive did-you-mean over every known name; NML5001
    /// for an argument the entry does not take or lacks; NML5002 for
    /// `#live` beside `#restart` on one field, when the package declares
    /// both. `source` is the text the models were extracted from — the
    /// did-you-mean's edit replaces from the `#` through the name.
    pub fn judge(&self, models: &[ModelDef], source: &str) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let names: Vec<&str> = self.entries().map(|e| e.name).collect();
        let declares = |name: &str| self.declared.iter().any(|d| d.name == name);
        let reload_pair = declares("live") && declares("restart");
        for model in models {
            for field in &model.fields {
                for directive in &field.directives {
                    self.judge_one(directive, source, &names, &mut out);
                }
                if reload_pair {
                    let live = field.directives.iter().find(|d| d.name == "live");
                    let restart = field.directives.iter().find(|d| d.name == "restart");
                    if let (Some(live), Some(restart)) = (live, restart) {
                        // The later of the two — the addition that created
                        // the contradiction.
                        let span = if restart.span.start > live.span.start {
                            restart.span
                        } else {
                            live.span
                        };
                        out.push(
                            Diagnostic::error("'#live' and '#restart' contradict — pick one")
                                .with_code(codes::DIRECTIVE_CONFLICT)
                                .with_span(span),
                        );
                    }
                }
            }
        }
        out
    }

    fn judge_one(
        &self,
        directive: &Directive,
        source: &str,
        names: &[&str],
        out: &mut Vec<Diagnostic>,
    ) {
        // An empty name means the parser already reported "expected a
        // directive name" on this token — an "unknown directive '#'" on top
        // helps no one.
        if directive.name.is_empty() {
            return;
        }
        let Some(entry) = self.get(&directive.name) else {
            let mut diag = Diagnostic::error(format!(
                "unknown directive '#{}' (package '{}')",
                directive.name, self.package_name
            ))
            .with_code(codes::UNKNOWN_DIRECTIVE)
            .with_span(directive.span);
            if let Some(suggested) =
                nml_core::suggest::suggest(&directive.name, names.iter().copied())
            {
                // The fix replaces from the `#` through the name with the
                // sigil-inclusive form, so the hint reads exactly what the
                // user types (`did you mean "#live"?`) — and applying it
                // normalizes any stray trivia between `#` and the name.
                let fix_span = Span::new(directive.span.start, name_span(directive, source).end);
                diag = diag.with_suggestion(
                    Suggestion::did_you_mean(format!("#{suggested}")).at(fix_span),
                );
            }
            out.push(diag);
            return;
        };
        let arity_error = match (entry.arg, directive.arg.is_some()) {
            (DirectiveArg::None, true) => Some(format!("'#{}' takes no argument", directive.name)),
            (DirectiveArg::None, false) => None,
            (_, false) => Some(format!("'#{}' requires an argument", directive.name)),
            (_, true) => None,
        };
        if let Some(message) = arity_error {
            out.push(
                Diagnostic::error(message)
                    .with_code(codes::DIRECTIVE_BAD_ARITY)
                    .with_span(directive.span),
            );
        }
    }
}

/// The byte span of a directive's *name* token. `Directive.span` covers the
/// whole construct (`#` through the close); the did-you-mean edit spans from
/// the `#` **through this span's end** (a sigil-inclusive replacement, never
/// touching an argument), so the precise question is where the name ends.
/// Located by searching the directive's own slice: the parser tolerates
/// trivia between `#` and the name, and a wrong end would mangle the
/// argument. A degenerate span degrades to the arithmetic fallback — never
/// a panic on a request path.
fn name_span(directive: &Directive, source: &str) -> Span {
    let span = directive.span;
    let fallback = Span::new(
        span.start + 1,
        (span.start + 1 + directive.name.len()).min(span.end),
    );
    let Some(slice) = source.get(span.start..span.end) else {
        return fallback;
    };
    let Some(rest) = slice.get(1..) else {
        return fallback;
    };
    match rest.find(&directive.name) {
        Some(rel) => {
            let start = span.start + 1 + rel;
            Span::new(start, start + directive.name.len())
        }
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::SchemaPackage;
    use crate::test_support::demo_package_with_directives;

    fn demo() -> Vocabulary {
        Vocabulary::new("demo", demo_package_with_directives().manifest.directives)
    }

    fn judge(source: &str, vocab: &Vocabulary) -> Vec<Diagnostic> {
        let (schema, errors) = nml_core::cst::extract_schema(source);
        assert!(errors.is_empty(), "{errors:?}");
        vocab.judge(&schema.models, source)
    }

    /// The four language directives are known under every vocabulary — a
    /// package that declares none, and one that declares its own.
    #[test]
    fn builtins_are_known_under_every_vocabulary() {
        let source = "model m:\n    a string #sealed\n    b []string #identity #append\n    c string #overlay\n";
        assert!(judge(source, &Vocabulary::new("bare", Vec::new())).is_empty());
        assert!(judge(source, &demo()).is_empty());
    }

    /// Builtins lead the entries, then the declared ones in order; `get`
    /// finds both kinds and reports which is which.
    #[test]
    fn entries_are_builtins_then_declared() {
        let vocab = demo();
        let names: Vec<&str> = vocab.entries().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "sealed", "identity", "append", "overlay", "live", "restart", "key"
            ]
        );
        assert!(
            vocab
                .get("sealed")
                .is_some_and(|e| e.builtin && e.arg == DirectiveArg::None)
        );
        assert!(
            vocab
                .get("key")
                .is_some_and(|e| !e.builtin && e.arg == DirectiveArg::Ident)
        );
        assert!(vocab.get("nope").is_none());
    }

    /// A near-miss of a BUILTIN gets the builtin as its did-you-mean, and the
    /// edit spans `#` through the name.
    #[test]
    fn a_near_miss_of_a_builtin_is_suggested() {
        let source = "model m:\n    a string #seled\n";
        let diags = judge(source, &Vocabulary::new("bare", Vec::new()));
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, Some(codes::UNKNOWN_DIRECTIVE));
        assert!(
            diags[0]
                .message
                .starts_with("unknown directive '#seled' (package 'bare')"),
            "{}",
            diags[0].message
        );
        let s = &diags[0].suggestions[0];
        assert_eq!(&source[s.span.start..s.span.end], "#seled");
        assert_eq!(s.replacement, "#sealed");
    }

    /// The declared vocabulary's own verdicts: unknown with the declared
    /// near-miss, arity both ways, and the reload pair only when declared.
    #[test]
    fn declared_verdicts_unknown_arity_and_conflict() {
        let vocab = demo();
        let diags = judge(
            "model m:\n    a string #lvie\n    b string #live(3)\n    c string? #key\n    d string #live #restart\n",
            &vocab,
        );
        let codes_seen: Vec<_> = diags.iter().map(|d| d.code).collect();
        assert_eq!(
            codes_seen,
            [
                Some(codes::UNKNOWN_DIRECTIVE),
                Some(codes::DIRECTIVE_BAD_ARITY),
                Some(codes::DIRECTIVE_BAD_ARITY),
                Some(codes::DIRECTIVE_CONFLICT)
            ],
            "{diags:?}"
        );
        assert_eq!(diags[0].suggestions[0].replacement, "#live");
        assert_eq!(diags[1].message, "'#live' takes no argument");
        assert_eq!(diags[2].message, "'#key' requires an argument");
        assert_eq!(
            diags[3].message,
            "'#live' and '#restart' contradict — pick one"
        );
        // In a vocabulary that declares neither, the pair is two unknown names, no conflict.
        let bare = judge(
            "model m:\n    d string #live #restart\n",
            &Vocabulary::new("bare", Vec::new()),
        );
        assert_eq!(bare.len(), 2);
        assert!(
            bare.iter()
                .all(|d| d.code == Some(codes::UNKNOWN_DIRECTIVE))
        );
    }

    /// The `#live`/`#restart` contradiction is a verdict only where the
    /// package declares BOTH: a vocabulary declaring `live` alone reports
    /// `#restart` as unknown and never a conflict.
    #[test]
    fn the_reload_pair_is_judged_only_when_both_are_declared() {
        let live_only = Vocabulary::new(
            "half",
            vec![DirectiveDecl {
                name: "live".to_string(),
                arg: DirectiveArg::None,
                doc: "Change applies without a restart.".to_string(),
            }],
        );
        let diags = judge("model m:\n    d string #live #restart\n", &live_only);
        assert_eq!(diags.len(), 1, "{diags:?}");
        assert_eq!(diags[0].code, Some(codes::UNKNOWN_DIRECTIVE));
        assert!(
            diags[0].message.starts_with("unknown directive '#restart'"),
            "{}",
            diags[0].message
        );
    }

    /// The conflict row squiggles the LATER of the two directives — the
    /// addition that created the contradiction — whichever it is.
    #[test]
    fn the_conflict_row_squiggles_the_later_directive() {
        let vocab = demo();
        let source = "model m:\n    d string #live #restart\n    e string #restart #live\n";
        let diags: Vec<_> = judge(source, &vocab)
            .into_iter()
            .filter(|d| d.code == Some(codes::DIRECTIVE_CONFLICT))
            .collect();
        assert_eq!(diags.len(), 2, "{diags:?}");
        let at = |d: &Diagnostic| {
            let s = d.span.expect("spanned");
            &source[s.start..s.end]
        };
        assert_eq!(at(&diags[0]), "#restart", "the later on the first field");
        assert_eq!(at(&diags[1]), "#live", "the later on the second field");
    }

    /// A manifest that declares a language directive under its own meaning
    /// is refused at load — NML2082 at the entry.
    #[test]
    fn a_manifest_redeclaring_a_builtin_is_nml2082() {
        let manifest = "package demo:\n    version = \"0.1.0\"\n    formatVersion = 1\n\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]directive directives:\n    - sealed:\n        arg = \"none\"\n        doc = \"Our own seal.\"\n";
        let err =
            SchemaPackage::from_parts(manifest, |_| Ok("model core:\n    a string\n".to_string()))
                .expect_err("refused");
        let crate::package::PackageError::Manifest { errors, .. } = err else {
            panic!("{err:?}");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, Some(codes::RESERVED_DIRECTIVE));
        assert!(
            errors[0].message.starts_with("`[]directive` entry 'sealed' redeclares the language's merge-policy directive `#sealed`"),
            "{}",
            errors[0].message
        );
        let at = errors[0].span.expect("at the entry").start;
        assert_eq!(manifest[at..].split(':').next().unwrap_or(""), "sealed");
    }
}
