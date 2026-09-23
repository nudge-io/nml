# nml-cli

Command-line tools for **NML**, a typed, indentation-based configuration
language. Installs the `nml` binary:

```bash
# until the crates.io release:
cargo install --locked --git https://github.com/nudge-io/nml nml-cli
```

```bash
nml parse <file>                  # parse and dump the AST as JSON (reports ALL errors)
nml validate <file>               # duplicate declarations + unresolved references
nml fmt [--check] <path>...       # format in place, canonical style (atomic write); --check for CI
nml check [--schema <dir>] <path>...  # CI-friendly: parse + validate + schema checks
                                      # (directories are walked; nml help <command> for any page)
nml fix [--dry-run] <path>...     # apply machine-applicable fixes in bulk (dirs walked)
nml binding <file>...             # which manifest binding governs a file (and its grant)
nml explain NML2007               # the full error-index entry, offline
nml <command> --help              # a command's options; every verb takes --json and -q/--quiet
```

Exit codes: 0 clean, 1 findings or a path that could not be checked or
fixed, 2 a usage error (an unknown flag, a missing argument) or an
invocation that contradicts the universe — the tree under the workspace
root, *closed* when a manifest there claims files by glob and *open* when
none does — in every verb. The command
list, as `nml help` prints it (every page wraps its option and exit-code
rows at 80 columns and lists its exit codes):

```text transcript=tests/fixtures/workspace-open
$ nml help
nml - NML configuration language toolkit

USAGE:
    nml <command> [options] <file>...
    nml <command> --help        a command's options (also: nml help <command>)

COMMANDS:
    parse <file>                Parse an NML file and dump the AST as JSON
                                (numbers: a JSON number for integer-form
                                values within u64, else an exact string)
    validate <path>...          Validate NML files for duplicates and
                                unresolved references (symbols only — no
                                schema validation); directories are walked
                                for .nml files
    fmt <path>...               Format NML files in place, canonical style
                                (spec/style.md); directories are walked for
                                .nml files; --check is the CI gate
    check <path>...             Parse + validate + schema check (CI-friendly);
                                directories are walked for .nml files; a
                                file a workspace manifest claims validates
                                under its binding's package and strictness
                                (then --schema is an error and --strict
                                does not apply); --strict makes unknown
                                properties and keywords errors for files no
                                binding governs
    fix <path>...               Apply machine-applicable fixes (migrations,
                                sole-candidate suggestions) in bulk;
                                directories are walked for .nml files;
                                --dry-run prints a diff
    binding <file>...           Show the binding, grant and universe
                                governing a file (exit 0 bound, 1 unbound
                                or ambiguous, 2 error)
    explain <code>...           Explain diagnostic codes (nml explain NML2007)
    explain --list              List every diagnostic code with its headline
    limits                      Print the toolkit's published bounds (what
                                tenant content can reach, and what only
                                operator input can)
    help                        Show this help message
    version                     Show version information

OPTIONS (`nml <command> --help` lists each command's own):
    --root <dir>                The workspace root every binding glob
                                anchors under (else derived from the first
                                target within its .git fence); CI should
                                pass it
    --json                      Line-delimited JSON on stdout (one object
                                per line, `type`-discriminated); nothing on
                                stderr
    -q, --quiet                 Errors only: warnings, infos, the explain
                                hint and a verb's success lines (the
                                per-file ok or fixed lines, the closing
                                tally) are not printed; findings, a verb's
                                answer, exit codes and the counts on the
                                closing row are untouched

EXIT CODES:
    0   clean (warnings do not fail a run)
    1   findings, a universe error, an unreadable target
    2   a usage error (an unknown command, a bad flag, a missing target),
        or an invocation that contradicts the universe (--schema beside a
        governed file)

EXAMPLES:
    nml check --root . tenants/                # every .nml file under tenants/
    nml fix --check --root . .                 # CI: fail if a fix would apply
    nml fmt --check --root . .                 # CI: fail if a file would change
    nml binding --root . tenants/cu/a.flow.nml # who governs a file, and why
    nml explain NML2087                        # the full entry for a code
```

`check`, `validate`, `fix` and `binding` accept `--root <dir>`: the
workspace root binding globs anchor under, fixed once per invocation
(else derived from the first target within its `.git` fence — refused
when a manifest sits above that fence or no fence is within 64
directories: pass `--root`). A file a
workspace manifest claims validates under that binding's package —
`--schema` beside a governing binding is a usage error (exit 2). They
take many targets under one universe, and `--json` turns stdout into
line-delimited JSON (stderr silent, exit codes unchanged). Only these
four verbs are safe over a tree an untrusted author commits to;
`parse` and `fmt` read and write the path as given.

`nml fmt` has no options: there is one canonical style, specified in
[`spec/style.md`](../spec/style.md) — what it regenerates (indentation,
spacing), what it preserves because it is the author's (blank lines, an
aligned `->` column, a string's delimiter, the line break before a value)
and what it refuses (a file that does not parse). That page also says how
to adopt the style on an existing tree: format and commit once, then turn
`--check` on.

`nml check --schema <dir>` loads every `*.model.nml` / `*.schema.nml` in the
directory and validates the file against them — exit code is non-zero on
errors, so it drops straight into CI:

```bash
nml check --schema schemas/ config.nml
```

The library APIs live in [`nml-core`](https://crates.io/crates/nml-core)
(parsing, query, serde) and
[`nml-validate`](https://crates.io/crates/nml-validate) (schema validation);
editor support is [`nml-lsp`](https://crates.io/crates/nml-lsp).

## Documentation

- [Language guide](https://github.com/nudge-io/nml/blob/main/docs/language-guide.md)
- [Integration guide](https://github.com/nudge-io/nml/blob/main/docs/integration.md)

## License

MIT OR Apache-2.0
