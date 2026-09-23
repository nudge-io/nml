//! The one argv parser every verb runs: `--root`, `--schema`, `--strict`,
//! `--dry-run`, `--check`, `--max-findings`, `--list`, `--json`,
//! `-q`/`--quiet`, `--help` and the positionals are read by ONE loop
//! against a per-verb [`Spec`], so no verb can spell a flag, an arity
//! rule or its help text differently. The usage line and the `--help`
//! page are COMPOSED from the spec, so they cannot drift from what the
//! parser accepts. A usage error — an unknown flag, a missing or surplus
//! positional, a flag without its value — is MARKED as one
//! (`out::usage_error`) so the run closes with exit 2 (clig.dev: 2 is
//! the invocation, 1 the domain), whatever the verb.
//!
//! `--help` / `-h` anywhere in a verb's arguments wins before any other
//! argument is read and BEFORE any filesystem access (clig.dev: help is
//! printed to stdout, exit 0) — never a file name that walks the
//! working directory's universe.

use std::path::PathBuf;

/// How many positionals a verb takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// One or more (`<path>...`).
    Many,
    /// Exactly one (`<file>`).
    One,
    /// One or more, or none when `--list` stands in for them (`explain`).
    ManyOrList,
    /// None (`limits`).
    None,
}

/// What a verb accepts.
pub struct Spec {
    pub verb: &'static str,
    /// One line: what the verb does (the `--help` headline).
    pub summary: &'static str,
    /// Whether the verb runs in a workspace universe (`--root <dir>`).
    pub root: bool,
    pub schema: bool,
    pub strict: bool,
    /// Whether the verb WRITES files, and so accepts `--dry-run` and
    /// `--check` (the CI gate) — and what those two flags say, which
    /// depends on what the verb writes ([`Edit`]).
    pub edit: Option<Edit>,
    /// Whether the verb accepts `--max-findings <n>` (the reporting
    /// budget; `0` lifts it).
    pub max_findings: bool,
    /// Whether the verb accepts `--list` in place of its positional.
    pub list: bool,
    /// The verb's positionals as the usage line spells them: `<path>...`
    /// for the verbs that walk directories, `<file>` for one file,
    /// `<code> | --list` for `explain`, empty for `limits`.
    pub targets: &'static str,
    pub arity: Arity,
    /// The verb's exit codes, one row each — `--help` documents them
    /// (clig.dev: "document exit codes in --help").
    pub exits: &'static [(&'static str, &'static str)],
    /// One or two realistic invocations — the most-read section of a
    /// help page (clig.dev: "lead with examples").
    pub examples: &'static [&'static str],
}

/// One accepted flag: its spelling in the usage line, its spelling in
/// the OPTIONS column and its `--help` explanation. Every verb draws from
/// this one table.
struct Flag {
    usage: &'static str,
    spelled: &'static str,
    help: &'static str,
}

const ROOT: Flag = Flag {
    usage: "[--root <dir>]",
    spelled: "--root <dir>",
    help: "the workspace root every binding glob anchors under (else derived from the \
           first target within its .git fence); CI should pass it",
};
const SCHEMA: Flag = Flag {
    usage: "[--schema <dir>]",
    spelled: "--schema <dir>",
    // "this run's schema sources", not "the schema universe": `universe`
    // is the open/closed workspace world everywhere else the reader meets
    // it (this same page's exit codes, `nml binding`, the `--json` row),
    // and two senses on one page is one too many.
    help: "load every *.model.nml / *.schema.nml in <dir> as this run's schema sources (a \
           usage error beside a file a workspace manifest governs)",
};
const STRICT: Flag = Flag {
    usage: "[--strict]",
    spelled: "--strict",
    help: "unknown properties and keywords are errors (closed-world config)",
};
/// What a writing verb writes — a fix or a reformat — so `--dry-run`
/// and `--check` say what THIS verb's dry run shows and what its gate
/// fails on, from one parser and one flag table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// `fix`: machine-applicable edits.
    Fix,
    /// `fmt`: the canonical text.
    Fmt,
}

impl Edit {
    fn dry_run(self) -> &'static Flag {
        match self {
            Edit::Fix => &DRY_RUN_FIX,
            Edit::Fmt => &DRY_RUN_FMT,
        }
    }

    fn check(self) -> &'static Flag {
        match self {
            Edit::Fix => &CHECK_FIX,
            Edit::Fmt => &CHECK_FMT,
        }
    }
}

const DRY_RUN_FIX: Flag = Flag {
    usage: "[--dry-run]",
    spelled: "--dry-run",
    help: "print the unified diff of every fix; write nothing",
};
const CHECK_FIX: Flag = Flag {
    usage: "[--check]",
    spelled: "--check",
    help: "CI gate: write nothing and exit 1 if any fix would apply or any error remains that \
           no fix repairs (implies --dry-run, so the diff prints). rustfmt, black, prettier \
           and gofmt spell this the same way",
};
const DRY_RUN_FMT: Flag = Flag {
    usage: "[--dry-run]",
    spelled: "--dry-run",
    help: "print the unified diff of every file not in canonical style; write nothing",
};
const CHECK_FMT: Flag = Flag {
    usage: "[--check]",
    spelled: "--check",
    help: "CI gate: write nothing and exit 1 if any file is not in canonical style (implies \
           --dry-run, so the diff prints). rustfmt, black, prettier and gofmt spell this the \
           same way",
};
/// UNPUBLISHED: not a numeric limit: the `--max-findings` FLAG's help text (the limit itself is nml-cli/src/out.rs::MAX_SHOWN)
const MAX_FINDINGS: Flag = Flag {
    usage: "[--max-findings <n>]",
    spelled: "--max-findings <n>",
    help: "print at most <n> findings (default 512, per-code fair); 0 prints them all. The \
           counts and the exit code are exact whatever the limit",
};
const LIST: Flag = Flag {
    usage: "",
    spelled: "--list",
    help: "print every diagnostic code with its one-line headline, instead of one code's \
           entry",
};
const JSON: Flag = Flag {
    usage: "[--json]",
    spelled: "--json",
    help: "line-delimited JSON on stdout (one object per line, `type`-discriminated); \
           nothing on stderr; exit codes unchanged",
};
const QUIET: Flag = Flag {
    usage: "[--quiet]",
    spelled: "-q, --quiet",
    help: "errors only: warnings, infos, the explain hint and a verb's success lines (the \
           per-file ok or fixed lines, the closing tally) are not printed; findings, a \
           verb's answer, exit codes and the counts on the closing row are untouched",
};

impl Spec {
    fn flags(&self) -> Vec<&'static Flag> {
        let mut flags = Vec::new();
        if self.root {
            flags.push(&ROOT);
        }
        if self.schema {
            flags.push(&SCHEMA);
        }
        if self.strict {
            flags.push(&STRICT);
        }
        if let Some(edit) = self.edit {
            flags.push(edit.dry_run());
            flags.push(edit.check());
        }
        if self.max_findings {
            flags.push(&MAX_FINDINGS);
        }
        if self.list {
            flags.push(&LIST);
        }
        flags.push(&JSON);
        flags.push(&QUIET);
        flags
    }

    /// Every LONG flag this verb accepts, `--help` included — read out of
    /// the same [`Flag`] table the usage line and the help page are
    /// composed from, so a near-miss is measured against what the parser
    /// really takes. `spelled` carries a value placeholder and a short
    /// form (`-q, --quiet`); only the `--`-leading words are
    /// candidates. Short flags are left out on purpose: every
    /// pair of one-letter flags is one edit apart, so `-x` would be
    /// answered with "did you mean `-h`?" — a guess with nothing behind
    /// it, and the one thing a did-you-mean must never be.
    fn long_flag_names(&self) -> Vec<&'static str> {
        let mut names = vec!["--help"];
        for flag in self.flags() {
            names.extend(
                flag.spelled
                    .split([' ', ','])
                    .filter(|w| w.starts_with("--")),
            );
        }
        names
    }

    /// The usage line, composed from the spec so it cannot drift from
    /// what the parser accepts.
    pub fn usage(&self) -> String {
        let mut s = format!("usage: nml {}", self.verb);
        for flag in self.flags().into_iter().filter(|f| !f.usage.is_empty()) {
            s.push(' ');
            s.push_str(flag.usage);
        }
        if !self.targets.is_empty() {
            s.push(' ');
            s.push_str(self.targets);
        }
        s
    }

    /// The verb's `--help` page: the usage line, the summary, one line
    /// per accepted flag, the exit codes, then examples. Ends with a
    /// newline.
    pub fn help(&self) -> String {
        let mut s = format!("{}\n\n", self.usage());
        wrapped(&mut s, String::new(), self.summary, 0);
        s.push_str("\nOPTIONS:\n");
        for flag in self.flags() {
            wrapped(&mut s, format!("    {:<20}", flag.spelled), flag.help, 24);
        }
        wrapped(
            &mut s,
            format!("    {:<20}", "-h, --help"),
            "print this help and exit 0",
            24,
        );
        if !self.exits.is_empty() {
            s.push_str("\nEXIT CODES:\n");
            for (code, meaning) in self.exits {
                wrapped(&mut s, format!("    {code:<4}"), meaning, 8);
            }
        }
        if !self.examples.is_empty() {
            s.push_str("\nEXAMPLES:\n");
            for example in self.examples {
                s.push_str(&format!("    {example}\n"));
            }
        }
        s
    }
}

/// The help page's column: every OPTIONS and EXIT CODES row (and the
/// summary) word-wraps at [`HELP_WIDTH`] columns, continuation lines
/// hanging at `hang` — a 150-column option line scrolled off every
/// terminal and CI log the page is read in. `first` is the row's
/// already-padded lead (`    --root <dir>        `).
const HELP_WIDTH: usize = 80;

fn wrapped(s: &mut String, first: String, text: &str, hang: usize) {
    let mut line = first;
    let mut col = line.chars().count();
    let mut fresh = true;
    for word in text.split_whitespace() {
        let width = word.chars().count();
        if !fresh && col + 1 + width > HELP_WIDTH {
            s.push_str(line.trim_end());
            s.push('\n');
            line = " ".repeat(hang);
            col = hang;
            fresh = true;
        }
        if !fresh {
            line.push(' ');
            col += 1;
        }
        line.push_str(word);
        col += width;
        fresh = false;
    }
    s.push_str(line.trim_end());
    s.push('\n');
}

/// A flag's value: the `--flag=value` spelling when the argument carried
/// one, else the next argument (`--flag value`) — both spellings, as
/// clig.dev asks.
fn take_value(
    args: &[String],
    i: &mut usize,
    flag: &str,
    inline: Option<&str>,
    missing: &str,
) -> Result<String, String> {
    if let Some(value) = inline {
        // `--root=` with nothing after the `=` is almost always an unset
        // shell variable (`--root=$ROOT` in CI): refused by name rather
        // than silently read as the working directory.
        if value.is_empty() {
            return Err(format!(
                "{missing} (`{flag}=` carries an empty value — an unset shell variable?)"
            ));
        }
        return Ok(value.to_string());
    }
    *i += 1;
    match args.get(*i) {
        // The separate spelling of the same mistake (`--root "$ROOT"`,
        // quoted, with ROOT unset): refused by name too, never read as
        // the working directory.
        Some(value) if value.is_empty() => Err(format!(
            "{missing} (`{flag} \"\"` carries an empty value — an unset shell variable?)"
        )),
        Some(value) => Ok(value.clone()),
        None => Err(missing.to_string()),
    }
}

/// A parsed invocation.
#[derive(Debug, Default)]
pub struct Invocation {
    pub root: Option<PathBuf>,
    pub schema: Option<PathBuf>,
    pub strict: bool,
    pub dry_run: bool,
    pub check: bool,
    pub json: bool,
    /// `-q`/`--quiet`: errors only.
    pub quiet: bool,
    /// `--list` in place of the positional.
    pub list: bool,
    /// `--max-findings <n>` as typed; `None` = the default budget.
    pub max_findings: Option<usize>,
    /// The positionals as typed, in order; never empty for a verb of
    /// [`Arity::Many`] or [`Arity::One`].
    pub targets: Vec<String>,
}

/// What an argument list asks for: a run, or the verb's help page.
#[derive(Debug)]
pub enum Parsed {
    Run(Invocation),
    Help(String),
}

impl Invocation {
    /// Parse `args` against `spec`. `Err` is the usage error's text,
    /// already marked as one (the run exits 2).
    pub fn parse(args: &[String], spec: &Spec) -> Result<Parsed, String> {
        Self::parse_unmarked(args, spec).map_err(crate::out::usage_error)
    }

    fn parse_unmarked(args: &[String], spec: &Spec) -> Result<Parsed, String> {
        // Help wins first, before any other argument is interpreted and
        // before anything touches the filesystem.
        if args.iter().any(|a| a == "--help" || a == "-h") {
            return Ok(Parsed::Help(spec.help()));
        }
        let mut inv = Self::default();
        let mut i = 0;
        // `--` ends flag parsing (POSIX; clig.dev): everything after it
        // is a positional, so a file named `--strict` can still be
        // checked.
        let mut only_targets = false;
        while i < args.len() {
            let arg = args[i].as_str();
            // Anything that starts with `-` is a flag (a bare `-` is a
            // file name): `-x` and `-qj` used to be taken as file names
            // and fail as "no such file", exit 1 — a usage error read
            // as a domain failure.
            if only_targets || !arg.starts_with('-') || arg == "-" {
                // An empty argument is no path at all: the invocation's
                // mistake, in every verb (`check ""` used to be a file
                // candidate that failed at the read, exit 1, while
                // `binding ""` was a usage error).
                if arg.is_empty() {
                    return Err(format!("an empty argument is not a path; {}", spec.usage()));
                }
                inv.targets.push(arg.to_string());
                i += 1;
                continue;
            }
            if arg == "--" {
                only_targets = true;
                i += 1;
                continue;
            }
            // `--flag=value` and `--flag value` are one flag.
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag, Some(value)),
                None => (arg, None),
            };
            match flag {
                "--root" if spec.root => {
                    let dir = take_value(
                        args,
                        &mut i,
                        flag,
                        inline,
                        "--root requires a directory argument",
                    )?;
                    inv.root = Some(PathBuf::from(dir));
                }
                "--schema" if spec.schema => {
                    let dir = take_value(
                        args,
                        &mut i,
                        flag,
                        inline,
                        "--schema requires a path argument",
                    )?;
                    inv.schema = Some(PathBuf::from(dir));
                }
                "--max-findings" if spec.max_findings => {
                    let raw = take_value(
                        args,
                        &mut i,
                        flag,
                        inline,
                        "--max-findings requires a count",
                    )?;
                    inv.max_findings = Some(
                        raw.parse::<usize>()
                            .map_err(|_| format!("--max-findings takes a count, not `{raw}`"))?,
                    );
                }
                "--strict" | "--dry-run" | "--check" | "--list" | "--json" | "--quiet" | "-q"
                    if inline.is_some() =>
                {
                    return Err(format!("{flag} takes no value; {}", spec.usage()));
                }
                "--strict" if spec.strict => inv.strict = true,
                "--dry-run" if spec.edit.is_some() => inv.dry_run = true,
                // `--check` IMPLIES `--dry-run` (rustfmt's spelling
                // exactly): one flag for CI, and passing both is the
                // same run.
                "--check" if spec.edit.is_some() => {
                    inv.check = true;
                    inv.dry_run = true;
                }
                "--list" if spec.list => inv.list = true,
                "--json" => inv.json = true,
                "--quiet" | "-q" => inv.quiet = true,
                _ => {
                    // A near-miss gets the crate's own did-you-mean —
                    // the engine `nml <typo>` already answers a mistyped
                    // VERB with. Without it `nml check --quite .` read
                    // the whole usage line back at a reader who had to
                    // spot the one changed letter themselves.
                    let hint = match flag
                        .strip_prefix("--")
                        .and_then(|_| nml_core::suggest::suggest(flag, spec.long_flag_names()))
                    {
                        Some(name) => format!(" (did you mean `{name}`?)"),
                        None => String::new(),
                    };
                    return Err(format!("unknown flag {arg}{hint}; {}", spec.usage()));
                }
            }
            i += 1;
        }
        let usage = spec.usage();
        match spec.arity {
            Arity::Many if inv.targets.is_empty() => return Err(usage),
            Arity::One if inv.targets.len() != 1 => return Err(usage),
            Arity::ManyOrList if inv.targets.is_empty() && !inv.list => return Err(usage),
            Arity::ManyOrList if !inv.targets.is_empty() && inv.list => {
                return Err(format!("--list takes no positional; {usage}"));
            }
            Arity::None if !inv.targets.is_empty() => {
                return Err(format!("unknown argument {}; {usage}", inv.targets[0]));
            }
            _ => {}
        }
        Ok(Parsed::Run(inv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECK: Spec = Spec {
        verb: "check",
        summary: "Parse + validate + schema check.",
        root: true,
        schema: true,
        strict: true,
        edit: None,
        max_findings: true,
        list: false,
        targets: "<file>...",
        arity: Arity::Many,
        exits: &[("0", "clean"), ("1", "findings")],
        examples: &["nml check --root . a.nml"],
    };

    const USAGE: &str = "usage: nml check [--root <dir>] [--schema <dir>] [--strict] \
                         [--max-findings <n>] [--json] [--quiet] <file>...";

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    fn run_spec(s: &[&str], spec: &Spec) -> Result<Invocation, String> {
        match Invocation::parse(&args(s), spec)? {
            Parsed::Run(inv) => Ok(inv),
            Parsed::Help(h) => panic!("help, not a run: {h}"),
        }
    }

    fn run(s: &[&str]) -> Result<Invocation, String> {
        run_spec(s, &CHECK)
    }

    #[test]
    fn one_parser_one_usage_line() {
        assert_eq!(CHECK.usage(), USAGE);
        let inv = run(&["--root", "r", "--strict", "x.nml", "--json", "y.nml"]).unwrap();
        assert_eq!(inv.root.as_deref(), Some(std::path::Path::new("r")));
        assert!(inv.strict && !inv.dry_run && inv.json && !inv.quiet);
        assert_eq!(inv.targets, ["x.nml", "y.nml"]);
        assert_eq!(
            run(&["--root"]).unwrap_err(),
            "--root requires a directory argument"
        );
        assert_eq!(run(&[]).unwrap_err(), USAGE);
        assert_eq!(
            run(&["--max-findings", "x", "a"]).unwrap_err(),
            "--max-findings takes a count, not `x`"
        );
        assert_eq!(
            run(&["--max-findings", "0", "a"]).unwrap().max_findings,
            Some(0)
        );
        // A flag a verb does not accept is unknown to it — the same
        // sentence in every verb.
        assert_eq!(
            run(&["--dry-run", "x"]).unwrap_err(),
            format!("unknown flag --dry-run; {USAGE}")
        );
    }

    /// A mistyped FLAG gets the did-you-mean a mistyped VERB has always
    /// got — the same engine, measured against the flags THIS verb takes
    /// (`--strict` is unknown to a spec without it, and suggests nothing
    /// there), and silent when nothing is close.
    #[test]
    fn a_mistyped_flag_names_the_flag_it_meant() {
        for (typo, meant) in [
            ("--quite", "--quiet"),
            ("--stricr", "--strict"),
            ("--roots", "--root"),
            ("--jsonl", "--json"),
            ("--maxfindings", "--max-findings"),
            ("--hepl", "--help"),
        ] {
            assert_eq!(
                run(&[typo, "x"]).unwrap_err(),
                format!("unknown flag {typo} (did you mean `{meant}`?); {USAGE}"),
            );
        }
        // Nothing close: the usage line alone, with no misleading guess.
        // `-x` and `-qj` are one edit from `-q`: a SHORT flag is never
        // answered with a guess.
        for typo in ["--zzzzzz", "--dry-run", "-x", "-qj", "-j"] {
            assert_eq!(
                run(&[typo, "x"]).unwrap_err(),
                format!("unknown flag {typo}; {USAGE}"),
                "{typo}: a flag this spec does not take suggests nothing",
            );
        }
        // The near-miss is measured on the flag, not on `--flag=value`.
        assert_eq!(
            run(&["--quite=1", "x"]).unwrap_err(),
            format!("unknown flag --quite=1 (did you mean `--quiet`?); {USAGE}")
        );
    }

    /// Both flag spellings (`--root=.` and `--root .`), `--` as the end
    /// of flags, and a value on a boolean flag refused by name.
    #[test]
    fn both_flag_spellings_and_double_dash() {
        let inv = run(&[
            "--root=r",
            "--max-findings=3",
            "--schema=s",
            "--",
            "--strict",
        ])
        .unwrap();
        assert_eq!(inv.root.as_deref(), Some(std::path::Path::new("r")));
        assert_eq!(inv.schema.as_deref(), Some(std::path::Path::new("s")));
        assert_eq!(inv.max_findings, Some(3));
        assert!(!inv.strict, "after `--`, `--strict` is a file name");
        assert_eq!(inv.targets, ["--strict"]);
        assert_eq!(
            run(&["--strict=yes", "x"]).unwrap_err(),
            format!("--strict takes no value; {USAGE}")
        );
        assert_eq!(
            run(&["--root=", "x"]).unwrap_err(),
            "--root requires a directory argument (`--root=` carries an empty value — an unset \
             shell variable?)",
            "an empty inline value is refused by name, never read as the working directory"
        );
        assert_eq!(
            run(&["--schema=", "x"]).unwrap_err(),
            "--schema requires a path argument (`--schema=` carries an empty value — an unset \
             shell variable?)"
        );
        // r86: the SEPARATE spelling of the same mistake (`--root "$ROOT"`,
        // quoted, with ROOT unset) is refused too — it read as the working
        // directory while `--root=` was refused.
        assert_eq!(
            run(&["--root", "", "x"]).unwrap_err(),
            "--root requires a directory argument (`--root \"\"` carries an empty value — an \
             unset shell variable?)"
        );
        assert_eq!(
            run(&["--schema", "", "x"]).unwrap_err(),
            "--schema requires a path argument (`--schema \"\"` carries an empty value — an \
             unset shell variable?)"
        );
    }

    /// `-q`/`--quiet` is a standard flag on every verb; after `--` the
    /// spelling is a file name.
    #[test]
    fn quiet_is_a_standard_flag() {
        assert!(run(&["-q", "x"]).unwrap().quiet);
        assert!(run(&["--quiet", "x"]).unwrap().quiet);
        assert_eq!(
            run(&["--quiet=1", "x"]).unwrap_err(),
            format!("--quiet takes no value; {USAGE}")
        );
        let inv = run(&["--", "-q"]).unwrap();
        assert!(!inv.quiet);
        assert_eq!(inv.targets, ["-q"]);
        // Any other `-`-prefixed spelling is a flag the verb does not
        // know (a usage error), never a file name; a bare `-` is a file.
        assert_eq!(
            run(&["-x", "x"]).unwrap_err(),
            format!("unknown flag -x; {USAGE}")
        );
        assert_eq!(
            run(&["-qj", "x"]).unwrap_err(),
            format!("unknown flag -qj; {USAGE}")
        );
        assert_eq!(run(&["-"]).unwrap().targets, ["-"]);
    }

    /// Every help row wraps at 80 columns with a hanging indent: no
    /// OPTIONS or EXIT CODES line runs past the terminal, and a wrapped
    /// row's continuation sits under its text column.
    #[test]
    fn help_rows_wrap_at_eighty_columns() {
        let h = CHECK.help();
        // The usage line is the one line that stays whole (a flag list a
        // reader copies); every row after it fits the terminal.
        for line in h.lines().skip(1) {
            assert!(line.chars().count() <= HELP_WIDTH, "{line:?}");
        }
        assert!(
            h.contains(
                "    --max-findings <n>  print at most <n> findings (default 512, per-code fair);\n                        0 prints them all. The counts and the exit code are\n                        exact whatever the limit\n"
            ),
            "{h}"
        );
    }

    /// The arities: exactly one file, at most one code (or `--list`),
    /// none at all — each refused in the usage line's own words; a verb
    /// with no universe does not accept `--root`.
    #[test]
    fn arities_and_the_list_flag() {
        const PARSE: Spec = Spec {
            verb: "parse",
            summary: "Parse.",
            root: false,
            schema: false,
            strict: false,
            edit: None,
            max_findings: false,
            list: false,
            targets: "<file>",
            arity: Arity::One,
            exits: &[],
            examples: &[],
        };
        const EXPLAIN: Spec = Spec {
            verb: "explain",
            summary: "Explain.",
            root: false,
            schema: false,
            strict: false,
            edit: None,
            max_findings: false,
            list: true,
            targets: "<code> | --list",
            arity: Arity::ManyOrList,
            exits: &[],
            examples: &[],
        };
        const LIMITS: Spec = Spec {
            verb: "limits",
            summary: "Limits.",
            root: false,
            schema: false,
            strict: false,
            edit: None,
            max_findings: false,
            list: false,
            targets: "",
            arity: Arity::None,
            exits: &[],
            examples: &[],
        };
        assert_eq!(PARSE.usage(), "usage: nml parse [--json] [--quiet] <file>");
        assert_eq!(run_spec(&["a.nml"], &PARSE).unwrap().targets, ["a.nml"]);
        assert_eq!(run_spec(&[], &PARSE).unwrap_err(), PARSE.usage());
        assert_eq!(run_spec(&["a", "b"], &PARSE).unwrap_err(), PARSE.usage());
        assert_eq!(
            run_spec(&["--root", ".", "a"], &PARSE).unwrap_err(),
            format!("unknown flag --root; {}", PARSE.usage())
        );
        assert_eq!(
            EXPLAIN.usage(),
            "usage: nml explain [--json] [--quiet] <code> | --list"
        );
        assert!(
            EXPLAIN
                .help()
                .contains("    --list              print every")
        );
        assert!(run_spec(&["--list"], &EXPLAIN).unwrap().list);
        assert_eq!(
            run_spec(&["NML2007"], &EXPLAIN).unwrap().targets,
            ["NML2007"]
        );
        assert_eq!(run_spec(&[], &EXPLAIN).unwrap_err(), EXPLAIN.usage());
        assert_eq!(
            run_spec(&["--list", "NML2007"], &EXPLAIN).unwrap_err(),
            format!("--list takes no positional; {}", EXPLAIN.usage())
        );
        assert_eq!(LIMITS.usage(), "usage: nml limits [--json] [--quiet]");
        assert!(run_spec(&["--json"], &LIMITS).unwrap().json);
        assert_eq!(
            run_spec(&["extra"], &LIMITS).unwrap_err(),
            format!("unknown argument extra; {}", LIMITS.usage())
        );
        assert_eq!(
            run_spec(&["--list"], &LIMITS).unwrap_err(),
            format!("unknown flag --list; {}", LIMITS.usage())
        );
    }

    /// The help page documents the exit codes and leads with an
    /// example, after the options (clig.dev).
    #[test]
    fn help_documents_exit_codes_and_examples() {
        let h = CHECK.help();
        let options = h.find("OPTIONS:").expect("options");
        let quiet = h
            .find("    -q, --quiet         errors only")
            .expect("the quiet row");
        let exits = h
            .find("EXIT CODES:\n    0   clean\n    1   findings\n")
            .expect("exit codes");
        let examples = h
            .find("EXAMPLES:\n    nml check --root . a.nml\n")
            .expect("examples");
        assert!(options < quiet && quiet < exits && exits < examples, "{h}");
    }

    /// `--help`/`-h` anywhere is the help page (stdout, exit 0 at the
    /// caller), before any other argument — even a bad one — is read.
    #[test]
    fn help_wins_before_a_bad_argument() {
        for argv in [
            &["--bogus", "--help"][..],
            &["-h"],
            &["a.nml", "--help", "--root"],
        ] {
            match Invocation::parse(&args(argv), &CHECK).unwrap() {
                Parsed::Help(h) => assert!(h.starts_with(USAGE), "{h}"),
                Parsed::Run(inv) => panic!("{argv:?} ran: {inv:?}"),
            }
        }
    }
}
