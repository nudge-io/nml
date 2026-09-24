# Validate in CI

The CLI is the CI surface: parse + validate + schema check in one command,
non-zero exit on error, every finding coded.

## Zero to a CI gate

A workspace an operator governs with a manifest, from nothing to a gate.

Step 1 is a manifest at the workspace root. `<name>.package.nml` names the
schemas and the `files` globs each validator binding claims. This is the
first binding of
[`tests/fixtures/workspace/demo.package.nml`](../../tests/fixtures/workspace/demo.package.nml),
the manifest several of this page's executed transcripts run against:

```nml fragment
package demo:
    version = "0.1.0"
    formatVersion = 1

[]schema schemas:
    - core:
        file = "core.model.nml"

[]validator validators:
    - tenantFlows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - core
        strict = true
```

Four commands cover the rest (the
[binding chapter](schema-packages-and-store.md#how-a-file-finds-its-binding-workspace-resolution)
explains what each does; `nml help <command>` prints any verb's page):

```bash
# 2. check everything under tenants/ — directories are walked, links and
#    dot-directories skipped; pass --root so CI and the editor agree on the universe
nml check --root . tenants/
# 3. the fixer's CI gate: write nothing, fail if a fix would apply or an error stands
nml fix --check --root . .
# 4. the formatter's CI gate: write nothing, fail if a file is not in canonical style
nml fmt --check --root . .
# 5. when a file is not judged as you expect, ask who governs it
nml binding --root . tenants/cu/flows/member-lookup.flow.nml
```

Exit 0 is green; anything else fails the job. A failing run ends with its
tally (`error: 1 error(s)`), and the line directly above it names the code
to look up (`for more information, run: nml explain NML2087`).
Prefer a directory target to a shell glob: `tenants/**/*.flow.nml` is
expanded by the *shell*, and bash without `shopt -s globstar` reads `**`
as `*` — one level deep, silently.

```bash
# Parse + symbol checks + schema validation against a schema directory:
nml check --schema schemas/ deploy.nml

# Unknown properties/keywords become ERRORS (closed-world config):
nml check --schema schemas/ --strict deploy.nml
```

A config that omits a required field:

```nml check schema=docs/guides/examples/cookbook/ci expect-error='[NML2007]'
server Main:
    host = "0.0.0.0"
```

And a clean one:

```nml check schema=docs/guides/examples/cookbook/ci
server Main:
    host = "0.0.0.0"
    port = 8080
```

(These two blocks run against [`examples/cookbook/ci/`](examples/cookbook/ci/)
in this repo's CI — the failure *and* the success are both verified.)

**Exit codes are a stable interface** ([stability policy](../stability.md)),
and every finding — universe findings included — carries a code:

| exit | `check` / `validate` | `fix` | `binding` |
|---|---|---|---|
| `0` | clean (warnings report but do not fail) | every path fixed or clean (a warning-only remainder included) | the file is bound and the run reported no error |
| `1` | any error: a finding in the file, a universe error (NML2081, NML2087–NML2089), a binding that cannot build its validator (NML2091 — a declared source fails to load; the file validates under no binding), a rejected path (NML2083), a target inside a denied budget unit (the unit's own NML2089 row), a file that cannot be read, a directory naming no `.nml` file, `.nml` content a directory walk skipped (NML2090 — one row per hidden directory; a walked symlinked `.nml` is NML2083) | a path that could not be fixed — absent, unreadable, or refused by the universe (a symlinked or ambiguously-claimed path, a denied tenant unit, a binding that cannot build its validator) — a universe error, **or (under `--check`) any fix that would apply, any error no fix repairs, or `.nml` content the directory walk skipped (NML2090)** | unbound or ambiguous, or any error the run reported — a universe error (NML2081, NML2087–NML2089) or a binding that cannot build its validator (NML2091) included — even where a binding still stands |
| `2` | a usage error — a target outside the workspace root included —, `--schema` beside a manifest-governed file or `--strict` with no schema to enforce (the invocation contradicts the universe), or a root that cannot be derived — a `.git` fence below a workspace manifest, no VCS root within 64 directories — pass `--root` | a usage error (a target outside the root included), the same `--schema` conflict, or the same refused derivation | a usage error, a directory target, the root cannot be derived, or the file is outside it |

**A usage error is exit 2 in every verb** — an unknown command or no
command at all, an unknown flag (`--bogus` or `-x`; a bare `-` is a file name), a missing or surplus argument, an empty argument (`""` — an unset shell variable where a path was meant), a flag without its value or with an empty one (`--root=` or `--root ""` — an unset shell variable), a `--root` that is no
directory, a `--schema` directory that cannot be read, a target outside the workspace root (`--root` or derived — the invocation names a universe the file is not in, so nothing runs), `--strict` with no schema to enforce, an argument that is not UTF-8, a root the tool refuses to
derive — so a script can tell "the file is wrong" (1) from "the
command is wrong" (2); `parse`, `explain`, `limits` and `version`
follow the same rule. Each names its reason in the tool's own words:

```text transcript=tests/fixtures/workspace-open
$ nml check --schema nope x.nml
error: --schema nope: no such directory
```
 A target INSIDE a budget unit the walk could not finish — an
unreadable directory, an entry flood — is the file's own failure: the
unit's NML2089 row, exit 1, in every verb, `binding` included. Under `--json` you do not have to re-encode this table:
**every verb's last row is a `summary` row carrying the run's `exit`**
(see below).

**The gate reports what the walk skipped.** A directory target expands
to the `.nml` files the universe walk enumerated — never a symlink, a
FIFO, a dot-file, or anything under a dot-directory, `node_modules` or
`target`. Every such entry rides the closing row's `skipped` list
(`[{key, why}]`, `why` a `skipReason`: `symlink`, `fifo`, `dotDirectory`,
`dotFile`, `policyDirectory`, `unkeyableName` — the entry's name is no
key (not UTF-8, or bearing a path separator), so the row's `key` is the
holding directory's and `entry` carries the name — or `componentBound`,
a directory at the 64-component bound, never listed), and
under a directory target `check`,
`validate` and `fix --check` FAIL on the `.nml` content among them
([NML2090](../../crates/nml-core/assets/error-index.md#nml2090); a
walked symlinked `.nml` under a closed universe is the resolver's own
NML2083) — a tree the gate certified is a tree it looked at whole. A
dot-directory holding `.nml` content is ONE row: the directory, the
exact count of `.nml` files beneath it and up to eight of their keys
(300,000 committed hidden files are one row, not 300,000). A symlinked
directory is a warning; a file target gates nothing.

Wire `0`/non-zero straight into CI; `--strict` upgrades unknown-name
findings to errors. The last line of an error run names the run's first
error code, else its first warning (`for more information, run: nml
explain NML2007`), once per run, and `nml explain --list` enumerates
every code your pipeline might meet.

**`nml fix --check` is the CI gate.** It writes nothing and exits 1 if
any fix would apply, or if an error remains that no fix repairs — a file
that does not parse, a path a closed binding rejects, a denied tenant
unit — the spelling `rustfmt`, `black`, `prettier` and `gofmt` all use
(they fail `--check` on a file they cannot parse, too). Pair it with
`nml check` for validity: the gate says whether the tree is fixed and
free of errors, `check` reports every finding and the warnings. It
implies `--dry-run`, so the diff prints and you can see exactly what the
tree is missing. `--dry-run` on its own keeps the Unix meaning ("show me
what would happen") and exits 0, so a script that pipes a dry run into
review tooling under `set -e` does not start failing the day the tree
grows a fixable finding.

```bash
nml fix --check --root . .        # CI: fails when the tree is unfixed
nml fix --dry-run --root . .      # human: show me, exit 0
```

**Diagnostic volume is bounded by default.** `check`, `validate` and
`fix` print at most 512 findings per run, allocated fairly
across diagnostic codes so one flooding code cannot crowd out a rarer
one, with a slice held back for codes that only appear late in a file.
**The counts and the exit code are exact whatever the limit** — a capped
run still says `error: 980000 error(s)` and still names, per code, what
it withheld. `--max-findings <n>` moves the limit; `--max-findings 0`
lifts it entirely and reproduces the full stream byte for byte; the
run says what it withheld, per code, on one `note:` line.

```text transcript=tests/fixtures/workspace
$ nml check --max-findings 1 --root . tenants/cu/bad.flow.nml
tenants/cu/nml-project.nml: warning[NML2080]: project config `tenants/cu/nml-project.nml` is inert: it sits inside content claimed by binding 'tenantFlows' of demo.package.nml (files[0] = "tenants/**/*.flow.nml") — content, not configuration; its pins, autoAssociate, bindings and anchoring are ignored
note: 2 more finding(s) not shown (limit 1; NML2001 ×1, NML2008 ×1) — pass --max-findings 0 to print them all
for more information, run: nml explain NML2008
error: 2 error(s)
```
 **A run over a set ends with one line.** A run that named a
directory, or more than one path, closes with `checked N file(s): N ok`
(`validated …` for `validate`) when every file passed, and with
`error: K of N file(s) failed` when any did not; a single file's own
`ok` line is its
whole verdict. **`-q`/`--quiet` prints errors only**, on every verb:
warnings, infos, the explain hint, the per-file `ok` lines, `fix`'s
per-file `fixed` lines and every closing tally are not printed;
findings, a verb's answer (a `binding` block, a `fix` diff, an
`explain` entry, the `--json` rows) and the exit code are untouched,
and the counts on the closing row stay exact — the way to keep a CI
log to what fails it:

```text transcript=tests/fixtures/workspace-units
$ nml check --root . tenants
tenants/cu/plain.flow.nml: ok (1 declaration(s))
tenants/du/plain.flow.nml: ok (1 declaration(s))
checked 2 file(s): 2 ok
$ nml check -q --root . tenants
```

**Choosing strictness:** run `--strict` on configs your own tool owns
end-to-end; leave it lenient for configs that downstream plugins may extend
(the [directive vocabulary recipe](directive-vocabulary.md) shows the
package-level way to keep even that closed). A file a workspace manifest
claims validates under its binding's own `strict` — in CI and in the
editor alike — so `--strict` does not apply to it: the run says so once
(`note: --strict does not apply to …`) and names the binding to set
`strict = true` on.

## Manifest-governed checks and `--root`

When the checked file sits under a workspace with a `<name>.package.nml`
(nudge RFC 0030), `nml check` resolves **which binding governs the file**
through the shared resolution core — one matcher, one selection rule,
the same core the editor runs — and validates under that binding's
package. Composition authority (RFC 0019 `layers:` grants) comes from
the same binding, so a file the manifest claims is judged identically by
every CLI verb and by the editor: an unbound file in a closed universe
(NML2064), a symlinked path (NML2083), an ambiguous claim (NML2087) and
a binding's `strict` are the same verdict, code and sentence in both.

- **`--root <dir>`** fixes the workspace root every binding glob anchors
  under. Without it the root is derived from the checked file: the
  outermost manifest or `nml-project.nml` between the file and its
  `.git` fence — and the derivation is fail-closed: a manifest or
  `nml-project.nml` ABOVE a fence that is no directory (a submodule's
  or linked worktree's `.git` file, a planted entry below the
  operator's manifest) refuses it, exit 2, naming both; above a `.git`
  DIRECTORY (a nested checkout, a stray manifest in a parent) the fence
  holds and the marker is disclosed; a `.git` FILE fence, or one
  shadowed by another `.git` or a marker above it, is disclosed once on
  stderr in human mode (`note: workspace root …`) and on the `--json`
  root object (`fence`, `shadowed` — the entry itself); no `.git`
  within 64 directories refuses too, and so does a shadow check the
  bound cut short. CI environments should pass `--root` explicitly — it
  pins the universe and rejects a file outside it.
- **A governed file validates under its binding's package.** Passing
  `--schema` for a file a manifest claims is a usage error (exit code 2)
  naming the manifest, the binding and the matched `files` glob: a CI
  flag cannot substitute another vocabulary inside a closed universe.
  Files no manifest claims keep the `--schema` behavior above.
- **`nml binding <file>`** prints the governing binding, its grant, the
  anchor and every inert input on the file's path (`NML2080`); exit
  codes are scriptable (0 bound, 1 unbound or ambiguous, 2 error).
- **Many targets, one universe.** `check`, `validate`, `fix` and
  `binding` take any number of targets; the root is fixed once (from
  `--root`, else the first target), the universe's own errors print once
  and fail the run before any target is opened, and each target then
  reports on its own — the run exits 1 if any failed. `check`, `validate`
  and `fix` expand a directory target to the `.nml` files the universe
  walk itself saw under it (one enumeration, the kernel's: only a real
  directory reached through no link at or below the root expands, a
  symlink, a FIFO, a dot-file and the policy-skipped `node_modules`,
  `target` and dot-directories are never among the files, a directory
  under a budget unit the walk stopped inside is reported as NML2089
  rather than entered, and a file reached through two spellings is taken
  once); targets are reported in argument order, each directory's files
  sorted within it, and a target that names no `.nml` file fails the
  run, naming it. `binding` takes files: on a directory it says so and
  exits 2.
- **`fmt` on a tree walks the universe as `check` and `fix` do.** A
  directory target, or `--root`, opens the same door: `nml fmt --check
  --root . .` is the formatter's CI gate — nothing is written, and a file
  not in canonical style (or `.nml` content the walk skipped, NML2090)
  fails the run; `--dry-run` shows the same diff and exits 0. A path a
  closed binding rejects is never opened, and a closed universe's rewrite
  lands at the key through the parent descriptor, exactly as `fix`'s does.
- **`parse`, and `fmt` on a bare file, run outside the walk.** A named
  file is formatted as rustfmt, black, prettier and gofmt format one —
  no tree is walked, so a file beside an unlistable neighbour (the system
  temp directory) still formats. Run these on your own files, not over a
  tree an untrusted author commits to. They keep one rule the universe
  would otherwise give them — the rule an OPEN universe's `fmt` and
  `fix` share on the way in: a symlinked leaf is resolved ONCE per
  invocation to the file it names — the same resolution serves the read
  and the write, so a link re-pointed while `fmt` runs still lands the
  rewrite on the file that was read — and that file is opened **beneath
  its own parent directory with `O_NOFOLLOW`** and must be a regular
  file — a FIFO, a device, a directory and a dangling link are refused
  at the open, nothing is created, and `fmt` rewrites the TARGET in
  place with the link standing, wherever the link points, inside or
  outside the root (what `rustfmt` and `gofmt` do). Links in the prefix you typed are followed, as every
  operator tool follows them, and a trailing separator on a file's
  name is insignificant on every verb (`f.nml/` names `f.nml`); a
  directory typed with one is still refused.
- **Bounds.** A check target is read up to 16 MiB — the editor indexes a
  workspace file under the same bound and says so past it, and refuses an
  open buffer past it with one row in the same sentence (nothing is
  parsed); a manifest or a
  project config up to 256 KiB; a declared schema source up to 4 MiB
  (every refusal names the bound). Discovery visits at most 65,536
  entries per budget unit (a tenant's directory under `tenants/**`) and
  1,048,576 in all, and reads at most 64 MiB of live inputs per unit
  and 1 GiB in all —
  see the [binding chapter](schema-packages-and-store.md#how-a-file-finds-its-binding-workspace-resolution)
  and, for the manifest itself, [tutorial 9](../tutorial/09-ship-schemas-to-your-users.md).

```bash
nml check --root . tenants/                                  # every .nml file under tenants/
nml check --root . tenants/cu-xyz/member-lookup.flow.nml
nml binding --root . tenants/cu-xyz/member-lookup.flow.nml
```

**How a root is derived without `--root`** — the shapes the derivation
meets, what each does, and how it is disclosed (with `--root` none of
this runs: the root is the flag):

| shape | outcome | exit | disclosed as |
|---|---|---|---|
| a `.git` directory fences the walk, nothing above it | the outermost manifest or `nml-project.nml` between the target and the fence anchors the universe; with none, the target's directory does (an open universe) | 0 | nothing — the plain checkout |
| a `.git` FILE fences the walk (a linked worktree, a submodule, a planted entry), nothing above it | the same derivation | 0 | `note: workspace root … (derived within a .git FILE fence …)` on stderr; `root.fence: "file"` |
| a `.git` DIRECTORY fence with a manifest or `nml-project.nml` above it (a nested checkout, a stray manifest in a parent) | the fence holds; the marker above is disclosed | 0 | `note: … SHADOWED by the root marker … — pass --root to pin, or --root <its directory> to check under that universe`; `root.shadowed` = the marker |
| another `.git` above the fence, no marker between | the fence holds; the outer `.git` is disclosed | 0 | `note: … SHADOWED by another .git entry …`; `root.shadowed` = that entry |
| a marker above a fence that is NO directory (a submodule's or worktree's `.git` file below the operator's manifest) | refused: `the root marker … sits above the fence at … and that fence is no directory …` | 2 | the `error:` line; `error.kind: "usage"` |
| the shadow check, or the fence search, climbs 64 directories without an answer | refused: `… reached the walk bound …` / `no VCS root within 64 directories …` | 2 | the `error:` line; `error.kind: "usage"` |
| no `.git` at all, up to the filesystem root | the target's own directory is the universe (open) — a manifest above it is never seen | 0 | `note: workspace root … (derived: no .git fence found, so the target's own directory is the workspace root — pass --root to pin)` on stderr; `root.origin: "derivedTargetDir"` |

`nml binding <file>` prints the same tag on its `root` line, and `-q`
keeps the `note:` silent. A refusal spells the marker, the fence and
the target from where you stand and names the `--root` that checks
under the manifest — here a `.git` FILE planted below the operator's
manifest (the docs test plants an empty one in a temporary copy):

```text transcript=tests/fixtures/workspace-units sparse=tenants/cu/.git:0
$ nml check tenants/cu/plain.flow.nml
error: cannot derive a workspace root for tenants/cu/plain.flow.nml: the root marker `demo.package.nml` sits above the fence at `tenants/cu/.git`, and that fence is no directory — a submodule's or linked worktree's .git file, or a planted entry, below a workspace manifest cannot shrink its universe (pass --root <dir>; --root . checks under that manifest)
```

## Terminal output and the locale

The sentences use a little typography — an em dash, an ellipsis — and
print it as UTF-8 when the locale's codeset is UTF-8 (`LC_ALL`, else
`LC_CTYPE`, else `LANG`, the first that is set; a Windows console
always). Under a `C`/`POSIX` or unset locale — a minimal container, a
`cron` job — the human output is ASCII instead: the tool's own
typography folds (`—` to `--`, `…` to `...`, and the other glyphs the
sentences and the error index use — `·` to `|`, `×` to `x`, `→` to
`->`, `≤` to `<=`) and any other non-ASCII character, a file name say,
is spelled `\u{XXXX}` rather than respelled (a name that uses one of
the tool's own glyphs folds with it). `NML_UNICODE=1` or `=0` forces
either rendering (cargo's `CARGO_TERM_UNICODE`). The `--json` stream is
UTF-8 whatever the locale, and a `fix --dry-run` diff is the file's own
bytes.

**Colour** paints the severity prefixes alone — `error[…]`,
`warning[…]`, `note:`, `help:`, the closing `error:` — in rustc's
palette, on stderr, and only when stderr is a terminal (`TERM` not
`dumb`): a pipe, a file or a CI log gets the same bytes it always did.
`NO_COLOR` (set and non-empty) turns it off anywhere and
`CLICOLOR_FORCE` (set, non-empty, not `0`) turns it on anywhere — a log
viewer that renders escapes — with `NO_COLOR` winning
([no-color.org](https://no-color.org/)). Paths, messages, a `binding`
block, a diff and the `--json` stream are never coloured.

## Machine-readable output: `--json`

`check`, `validate`, `fix`, `binding`, `limits`, `explain`, `version`, `parse`
and `fmt` accept `--json`: stdout becomes
line-delimited JSON — one object per line, discriminated by `type` — and
stderr stays silent. The stream describes itself before it says
anything else: its FIRST row is `contract` —
`{"type":"contract","formatVersion":1,"revision":3,"nmlVersion":"…"}` —
`formatVersion` this contract's number (moved by a rename or a removal,
never by an addition), `revision` the additions within it (moved by
every new key, row type or enumerated value), `nmlVersion` the binary
that wrote the stream; the closing `summary` row repeats the three, so
a consumer holding only a log's tail still knows what it reads.
`nml version --json | head -1` reads all three with no target at all.
Pin `formatVersion`; validate strictly only against the schema at the
`revision` the header names (the schema pins it as a constant on that
row, and the row is first, so a stricter consumer fails on the contract
row, never on a finding); otherwise ignore keys you do not know and
keep a default arm on enumerated values ([the stability
policy](../stability.md) states the rule). **The
contract is a published JSON Schema**, `docs/json/nml-ndjson-v1.schema.json`
(this repository's `docs/json/`): every row type and every value
vocabulary, every row closed (`additionalProperties: false`),
`formatVersion` and `revision` constants — `scripts/docs_test.py`
validates every executed transcript's `--json` rows against it and
that every stream opens with `contract` and closes with `summary`, and
a saved CI log can be validated the same way. The CHANGELOG names each
revision's additions in one entry (`--json formatVersion 1, revision
1`, …), so what a newer stream added is one lookup. Columns (`col`,
`endCol`) are 1-based byte columns of the line. **One
vocabulary:** field names are
lowerCamel, and every enumerated value is one lowerCamel word —
`root.origin` is `explicit`, `editor`, `derivedVcsFence` or
`derivedTargetDir` (a walk that meets no fence within 64 directories
refuses instead of deriving); `root.fence` is the fence entry's kind —
`dir`, `file`, `symlink`, `other` — or null, and `root.shadowed` the
entry above the fence that shadows the derived universe — another
`.git`, or a manifest above a directory fence — or null; `binding.step` is
`pinned` or `autoAssociated`; `binding.class` is `workspace`,
`injected`, `store` or `builtin`; `truncatedUnits[].why` is `entries`,
`liveInputBytes` or `unreadable`; `skipped.byWhy`'s keys and
`skipped.rows[].why` are a `skipReason`: `symlink`, `fifo`,
`dotDirectory`, `dotFile`, `policyDirectory`, `unkeyableName` (the
entry's name is no key; that row carries `entry`, the name) or
`componentBound` (a directory at the 64-component bound, never listed);
`closure` is
`complete`, `truncated` or `unloadable`;
`universe` is `closed` or `open`; `governing` is `bound`, `unbound` or
`ambiguous`; `severity` is `error`, `warning` or `info`; `error.kind` is
`usage`, `target` or `run`. Exit codes are unchanged, so a pipeline
keeps its `set -e` semantics and parses the rows for detail; `--quiet`
drops the warning and info rows and keeps the counts. No human text is
ever mixed in, a usage error included (a `--help` page is output, not a
run: it prints alone, with no row). A consumer that closes the pipe
early (`| head -1`) ends the run silently — no panic, nothing on stderr,
exit 1.

| `type`       | verbs                    | fields |
|--------------|--------------------------|--------|
| `contract`   | all                      | `formatVersion` (`1`), `revision` (`3` — the additions within the format), `nmlVersion` — **always the first row of the RUN**, before any finding or answer; the closing `summary` row carries the same three |
| `diagnostic` | check, validate, fix, binding (`notes[]`) | `source`, `line`, `col` (null when locationless), `severity` (`error`/`warning`/`info`), `code` (`"NML2064"` or null), `message`, `related[]` (`{source, line, col, message}`), `suggestions[]` (`{kind, source, edits[{line, col, endLine, endCol, lines}]}` — the finding's machine-applicable edits, resolved against the files as the run read them: `kind` one of `didYouMean`/`fix`/`delete`/`insert`, `source` the edited file's key — NML2064's `layers:` block goes in the manifest — and each edit the text from `line:col` to `endLine:endCol` replaced by `lines` joined with a newline; a consumer applying one later first verifies the file is as the run read it), `cause` (`{code, source, line, col, message}` — only on a row that reports another finding's refusal: NML2088 over the manifest's first finding, NML2091 over a declared source's; the underlying code as a fact, its place in its own file; absent elsewhere, so `cause?.code ?? code` is the code to act on) |
| `result`     | check, validate          | `verb`, `target` (as typed), `key` (workspace-relative), `ok`, `errors`, `warnings`, `declarations` (null for validate) — always the last row for its TARGET |
| `summary`    | all                      | `formatVersion`, `revision`, `nmlVersion` (as on the `contract` row), `verb`, `exit` (the process's own exit code), `targets` (the files after directory expansion), `errors`, `warnings` (exact, `--quiet` or not), `root{path, origin, fence, shadowed}` (`origin` one of `explicit`/`editor`/`derivedVcsFence`/`derivedTargetDir`; `fence` the fence entry's kind `dir`/`file`/`symlink`/`other` or null; `shadowed` the entry above the fence that shadows the derived universe — another `.git`, or a manifest above a directory fence — or null), `universe` (`closed`/`open`), `closure` (`complete`/`truncated`/`unloadable` — the kernel's word on the whole universe), `manifests`, `truncatedUnits` (`[{unit, stop, why}]` — the budget units the walk stopped inside, each denied in full while the rest of the universe stands, `why` one of `entries`/`liveInputBytes`/`unreadable`; empty when none, null without a universe), `skipped` (`{byWhy, rows, shown, hidden}` — what the walk left out of its enumeration by policy: `byWhy` the EXACT count per reason, `symlink`/`fifo`/`dotDirectory`/`dotFile`/`policyDirectory`/`unkeyableName`/`componentBound`; `rows` `[{key, why}]` by depth then key, an `unkeyableName` row also carrying `entry` (the name no key can carry, under the holding directory's `key`), the first `--max-findings` of them (`0` lifts the bound) and `hidden` how many rows that budget held back; null without a universe), `withheld` (`{shown, hidden, byCode}` or null — what the finding-PRINTING budget held back; `--quiet`'s silence is not counted here) — **always the last row of the RUN**, on every path: a clean run, a failing one, a parse error, an absent target, a usage error. `fix` carries its own fields (`dryRun`, `edits`, `filesFixed`, `files`, `remaining`, `routed` — the distinct edits its findings carry that lie in another file, refused here and pending there, a part of `remaining` — `suppressed`, `budgetExhausted`, `failed`) in the same row, on every path — zeros when no `.nml` file was found; `fmt` carries `dryRun` the same way |
| `limit`      | limits                   | `name` (module-qualified), `value`, `reach` (`content`/`peer`/`operator`/`internal`), `guards` (`work`/`memory`/`output`/`domain`), `surface` (`kernel`/`cli`/`editor`), `what`, `declaredIn`, `declaration`, `published` |
| `binding`    | binding                  | `file` (key), `absent` (the path the walk verified and found no leaf for — the binding shown is what WOULD govern it), `root{path, origin, fence, shadowed}` (as on `summary`), `universe` (`closed`/`open`), `closure`, `manifests`, `truncatedUnits` (as on `summary`), `governing` (`bound`/`unbound`/`ambiguous`), `binding{name, package, contentHash, class (workspace/injected/store/builtin), manifest, anchor, glob{index, pattern}, step (pinned/autoAssociated)}` or null, `layers{granted, allowRefs[], denyRefs[], maxStackDepth}`, `claimants[]` (ambiguous only), `notes[]` (diagnostic rows; under `--quiet` the errors among them) |
| `fix`        | fix                      | `file`, `applied`, `remaining`, `routed` (edits of this file's findings that lie in another file — pending there), `dryRun`, `diff` (unified diff text on a dry run, else null) |
| `parse`      | parse                    | `file`, `ast` (the AST as data — the pretty dump is the human form) |
| `fmt`        | fmt                      | `file`, `changed` (whether the file's bytes differ from canonical — on a real run the rewrite changed them, on a dry run it would) — one row per file |
| `explain`    | explain                  | `code`, `summary`, `document` — one row per code (`nml explain A B …` emits one each, `--list` every code; an unknown code among many is an `error` row of kind `target`) |
| `version`    | version                  | `version` — the binary's version (the `contract` and `summary` rows carry it as `nmlVersion` too) |
| `error`      | all                      | `kind` (`usage` — the invocation itself: a bad flag, a missing target, the `--schema` conflict, exit 2; `target` — one target of a many-target run failed and the run went on; `run` — the closing verdict, emitted only when no `result`/`summary` row carries it), `message`, `exit` |

```bash
nml check --json --root . tenants/cu/member-lookup.flow.nml \
  | jq -r 'select(.type=="diagnostic" and .severity=="error") | "\(.source):\(.line): \(.code) \(.message)"'
```

Every row type above, executed over this repository's `workspace`
fixture (`${ROOT}` stands for the fixture's absolute path and
`${VERSION}` for the binary's version — the docs test substitutes both
and compares the rest byte for byte): a governed file under an inert
tenant config (a `diagnostic` warning, its `result`, the `summary`), a
denied composition whose `diagnostic` carries the remedy as a resolved
edit in the manifest (`suggestions[]`), an
ambiguously claimed file (`binding`, then the `summary` carrying exit 1),
an absent target (an `error` row of kind `run`, then the `summary`) and
`nml version` (the `contract` row, one `version` row, then the `summary`
— the smallest stream there is; every one of them opens with the
`contract` row).

```text transcript=tests/fixtures/workspace
$ nml check --json --root . tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"code":"NML2080","col":null,"line":null,"message":"project config `tenants/cu/nml-project.nml` is inert: it sits inside content claimed by binding 'tenantFlows' of demo.package.nml (files[0] = \"tenants/**/*.flow.nml\") — content, not configuration; its pins, autoAssociate, bindings and anchoring are ignored","related":[],"severity":"warning","source":"tenants/cu/nml-project.nml","suggestions":[],"type":"diagnostic"}
{"declarations":1,"errors":0,"key":"tenants/cu/plain.flow.nml","ok":true,"target":"tenants/cu/plain.flow.nml","type":"result","verb":"check","warnings":0}
{"closure":"complete","errors":0,"exit":0,"formatVersion":1,"manifests":2,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":1,"withheld":null}
$ nml check --json --root . tenants/cu/member-lookup.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"code":"NML2080","col":null,"line":null,"message":"project config `tenants/cu/nml-project.nml` is inert: it sits inside content claimed by binding 'tenantFlows' of demo.package.nml (files[0] = \"tenants/**/*.flow.nml\") — content, not configuration; its pins, autoAssociate, bindings and anchoring are ignored","related":[],"severity":"warning","source":"tenants/cu/nml-project.nml","suggestions":[],"type":"diagnostic"}
{"code":"NML2064","col":7,"line":4,"message":"composition not permitted: binding 'tenantFlows' (demo.package.nml) carries no `layers:` grant — an operator change, not fixable from a content file; run `nml binding tenants/cu/member-lookup.flow.nml` to see the effective grant","related":[{"col":7,"line":10,"message":"to permit it, give this binding a `layers:` grant whose `allowRefs` admits \"tenants/cu/member-lookup.flow.nml\"","source":"demo.package.nml"}],"severity":"error","source":"tenants/cu/member-lookup.flow.nml","suggestions":[{"edits":[{"col":1,"endCol":1,"endLine":16,"line":16,"lines":["        layers:","            allowRefs:","                - \"tenants/cu/member-lookup.flow.nml\"",""]}],"kind":"insert","source":"demo.package.nml"}],"type":"diagnostic"}
{"declarations":2,"errors":1,"key":"tenants/cu/member-lookup.flow.nml","ok":false,"target":"tenants/cu/member-lookup.flow.nml","type":"result","verb":"check","warnings":0}
{"closure":"complete","errors":1,"exit":1,"formatVersion":1,"manifests":2,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":1,"withheld":null}
$ nml binding --json --root . shared/x.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"absent":false,"binding":null,"claimants":[{"anchor":".","class":"workspace","contentHash":"blake3:be06ba4bc705bbc16e1ed75caed75ce74e76b4f4559c6caef71428f3a0cd006d","glob":{"index":0,"pattern":"shared/**/*.flow.nml"},"manifest":"demo.package.nml","name":"shared","package":"demo"},{"anchor":".","class":"workspace","contentHash":"blake3:33f4be383a9fe23ce15887e569d23f903dfa8de0729af501a67d4f734d1632c3","glob":{"index":0,"pattern":"shared/**/*.flow.nml"},"manifest":"other.package.nml","name":"sharedToo","package":"other"}],"closure":"complete","file":"shared/x.flow.nml","governing":"ambiguous","layers":{"granted":false},"manifests":2,"notes":[{"code":"NML2087","col":null,"line":null,"message":"2 manifests claim this file: demo.package.nml (shared, files[0] = \"shared/**/*.flow.nml\"), other.package.nml (sharedToo, files[0] = \"shared/**/*.flow.nml\") — an ambiguously-claimed file is denied: it validates under no binding and nothing runs against it; remove or narrow one claim (a `schemaPackages` pin in the nearest live project config chooses between package names, never between two manifests of one name)","related":[],"severity":"error","source":"shared/x.flow.nml","suggestions":[],"type":"diagnostic"}],"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"truncatedUnits":[],"type":"binding","universe":"closed"}
{"closure":"complete","errors":1,"exit":1,"formatVersion":1,"manifests":2,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"binding","warnings":0,"withheld":null}
$ nml check --json --root . nope.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"exit":1,"kind":"run","message":"nope.nml: no such file or directory","type":"error"}
{"closure":"complete","errors":0,"exit":1,"formatVersion":1,"manifests":2,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":1,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
$ nml version --json
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"type":"version","version":"${VERSION}"}
{"closure":null,"errors":0,"exit":0,"formatVersion":1,"manifests":null,"nmlVersion":"${VERSION}","revision":${REVISION},"root":null,"schemaSources":null,"skipped":null,"targets":0,"truncatedUnits":null,"type":"summary","universe":null,"verb":"version","warnings":0,"withheld":null}
```

A row that reports another finding's refusal under a code of its own
carries that finding as `cause` — the underlying code as a fact, its
place in its own file — so a consumer never parses the sentence to
tell a repeated entry from an unknown property or a parse error:
`cause?.code ?? code` is the code to act on, and the finding's machine
edit, when it has one (a did-you-mean in the manifest), rides the
row's `suggestions[]` in the manifest's file. Here a manifest that
names `files` twice fails to load (NML2088) over the repeated entry
(NML2093), executed over the `workspace-dup` fixture; NML2091 carries
the declared source's first finding the same way, in the source's
file. A row that IS the finding — reported under the inner finding's
own code (NML2081), or refused by a sentence with no finding behind it
(a source that is absent, a manifest past its byte cap) — carries no
`cause` key; a rule of the loader's own (NML2094–NML2104) rides as the
cause like any other finding.

```text transcript=tests/fixtures/workspace-dup
$ nml check --json --root . tenants/cu/plain.flow.nml
{"formatVersion":1,"nmlVersion":"${VERSION}","revision":${REVISION},"type":"contract"}
{"cause":{"code":"NML2093","col":9,"line":16,"message":"duplicate entry 'files' — a body declares each name once (`files:` and `files = …` are two spellings of one entry)","source":"demo.package.nml"},"code":"NML2088","col":9,"line":16,"message":"manifest failed to load: duplicate entry 'files' — a body declares each name once (`files:` and `files = …` are two spellings of one entry)","related":[{"col":9,"line":11,"message":"'files' first declared here","source":"demo.package.nml"}],"severity":"error","source":"demo.package.nml","suggestions":[],"type":"diagnostic"}
{"exit":1,"kind":"run","message":"1 error(s)","type":"error"}
{"closure":"unloadable","errors":1,"exit":1,"formatVersion":1,"manifests":1,"nmlVersion":"${VERSION}","revision":${REVISION},"root":{"fence":null,"origin":"explicit","path":"${ROOT}","shadowed":null},"schemaSources":null,"skipped":{"byWhy":{},"hidden":0,"rows":[],"shown":0},"targets":0,"truncatedUnits":[],"type":"summary","universe":"closed","verb":"check","warnings":0,"withheld":null}
```

## Published bounds: `nml limits`

`nml limits` prints every *published* bound — what it bounds,
its value, **who can reach it** (`content` a tenant commits, the editor's
`peer`, or only the `operator` — a class with no member today: every
bound sits behind a file the operator points the tool at), **what it
guards** (`work`, `memory`, `output`, or a `domain` ceiling) and **where
it lives** (`kernel`, `cli`, `editor`). `--json` emits one `limit` row per
bound, plus the bounds deliberately left unpublished with the reason for
each. Each bound's classification is a `LIMIT:` line in the constant's
own doc comment; a census test fails the build when the table and the
doc line disagree, when a new bound appears unclassified, when a
published value drifts, or when a published bound is named by no test —
and the table below is generated from `nml limits --json` by the docs
test, so none of this can rot.

```bash
nml limits
nml limits --json | jq -r 'select(.reach=="content") | "\(.name)\t\(.value)"'
```

<!-- nml limits: begin — GENERATED by scripts/docs_test.py from `nml limits --json`; NML_UPDATE_GOLDEN=1 rewrites it, never edit by hand -->
| bound | value | reach | guards | surface | what |
|---|---|---|---|---|---|
| `nml-core::cst::lexer::MAX_SOURCE_LEN` | 4294967295 bytes | content | domain | kernel | the bytes one source document may hold (the span index is 32-bit) |
| `nml-core::cst::parser::MAX_DEPTH` | 64 | content | memory | kernel | nesting depth of blocks, bodies and values the parser will descend, and arms in one fallback chain |
| `nml-core::cst::value::MAX_JUDGED_TOKEN_BYTES` | 64 KiB | content | work | kernel | bytes of ONE token the source-character policy will judge |
| `nml-core::decimal::COEFF_ABS_MAX` | 10^34 - 1 | content | domain | kernel | the coefficient magnitude of an exact decimal (34 significant digits) |
| `nml-core::defaults::MAX_DEFAULT_DEPTH` | 64 | content | memory | kernel | nesting depth when a model's defaults are materialized |
| `nml-core::defaults::MAX_MATERIALIZED_MODELS` | 1024 | content | memory | kernel | models materialized for defaults in one pass |
| `nml-core::defaults::MAX_REFERENCE_DEPTH` | 16 | content | memory | kernel | chained default references followed |
| `nml-core::diagnostic::MAX_ERRORS` | 128 | content | output | kernel | parse/lex findings REPORTED per document; the rest are counted, never dropped silently |
| `nml-core::diff::MAX_DEPTH` | 64 | content | memory | kernel | nesting depth the structural diff will descend |
| `nml-core::duration::MAX_SEGMENTS` | 6 | content | domain | kernel | segments in one compound duration literal (`1h30m`) |
| `nml-core::duration::STD_MAX_NANOS` | ~1.8e28 ns | content | domain | kernel | the largest duration representable as a `std::time::Duration` |
| `nml-core::error::MAX_ECHO` | 32 | content | output | kernel | source characters a diagnostic echoes back at you |
| `nml-core::error::MAX_FIX_CAPTURE` | 64 | content | output | kernel | bytes of source one machine-applicable fix may capture |
| `nml-core::identity::MAX_POSITIONAL_DEPTH` | 64 | content | memory | kernel | nesting depth when a positional identity is derived |
| `nml-core::layers::MAX_STACK_DEPTH` | 16 | content | work | kernel | `uses` composition stack depth (a binding grant may lower it, never raise it) |
| `nml-core::resolve::MAX_RESOLVE_DEPTH` | 64 | content | memory | kernel | reference-resolution depth |
| `nml-core::suggest::MAX_INPUT_LEN` | 256 | content | work | kernel | name length the did-you-mean engine will consider |
| `nml-core::symbols::SUGGESTION_BUDGET` | 128 | content | work | kernel | unresolved references that carry a did-you-mean; every one is still reported |
| `nml-validate::fs::MAX_MANIFEST_BYTES` | 256 KiB | content | memory | kernel | bytes of one package manifest or project config read (past it the input is refused, NML2088) |
| `nml-validate::fs::MAX_SOURCE_BYTES` | 4 MiB | content | memory | kernel | bytes of one schema source read (past it the input is refused, NML2088) |
| `nml-validate::glob::MAX_PATTERN_SEGMENTS` | 64 | content | work | kernel | segments in one manifest `files` glob |
| `nml-validate::glob::MAX_PATTERN_SEGMENT_BYTES` | 1 KiB | content | work | kernel | bytes of ONE segment of a manifest glob (`files`, `allowRefs`, `denyRefs`, `budgetUnits`) |
| `nml-validate::glob::MAX_SUBSUMES_STATES` | 10000 | content | work | kernel | states explored when deciding whether one glob subsumes another |
| `nml-validate::package::MAX_SHADOW_WORK` | 20000000 | content | work | kernel | subsumption work one manifest's shadow analysis may spend across every glob pair it compares |
| `nml-validate::package::MAX_GLOB_ECHO_BYTES` | 160 bytes | content | output | kernel | bytes of a manifest glob a loader finding echoes before eliding its middle |
| `nml-validate::schema::MAX_VALIDATION_DEPTH` | 64 | content | memory | kernel | nesting depth of instance validation against a model |
| `nml-validate::schema::MAX_FIX_ALTERNATIVES` | 8 | content | output | kernel | mutually exclusive fix alternatives offered for one finding |
| `nml-validate::schema::MAX_ECHO` | 32 | content | output | kernel | source characters a schema diagnostic echoes back at you |
| `nml-validate::store::MAX_POINTER_BYTES` | 4 KiB | content | memory | kernel | bytes of a schema package's `current` store pointer the loader reads |
| `nml-validate::workspace::discover::MAX_ENTRIES` | 65536 | content | work | kernel | directory entries one BUDGET UNIT (a directory at a claiming glob's unit boundary — the start of its last run of wildcard directory segments — else the root) may spend before the walk truncates that unit alone (NML2089) |
| `nml-validate::workspace::discover::MAX_TOTAL_ENTRIES` | 1048576 | content | work | kernel | directory entries one universe walk visits in total, every budget unit summed, before the WHOLE universe is truncated (NML2089) |
| `nml-validate::workspace::discover::MAX_LIVE_INPUT_BYTES` | 64 MiB | content | memory | kernel | bytes of live manifests, live project configs and DISTINCT declared schema sources one BUDGET UNIT may read before the unit is denied (NML2089); the root unit's are the universe's |
| `nml-validate::workspace::discover::MAX_TOTAL_LIVE_INPUT_BYTES` | 1 GiB | content | memory | kernel | bytes of live manifests, live project configs and DISTINCT declared schema sources one universe walk reads in total, every budget unit summed, before the WHOLE universe is truncated (NML2089) |
| `nml-validate::workspace::discover::MAX_AUDIT_EXAMPLES` | 8 | content | output | kernel | example keys a hidden-directory NML2090 row names beside its exact count |
| `nml-validate::workspace::paths::MAX_COMPONENTS` | 64 | content | domain | kernel | path components in a source key, a root walk or an authored path |
| `nml-cli::fix::MIN_ROUNDS` | 8 | content | work | cli | floor on fix rounds per file |
| `nml-cli::fix::MAX_ROUNDS` | 64 | content | work | cli | fix rounds per file; a file that hits it says so and a second run continues |
| `nml-cli::fix::MAX_CELLS` | 4000000 | content | work | cli | cells in the `--dry-run` diff matrix before it falls back to a coarser diff |
| `nml-cli::out::MAX_SHOWN` | 512 | content | output | cli | findings PRINTED per run (`--max-findings`); the counts stay exact |
| `nml-cli::workspace::MAX_TARGET_BYTES` | 16 MiB | content | memory | cli | bytes of a check/validate/fix TARGET |
| `nml-lsp::diagnostics::MAX_DIAGNOSTICS` | 500 | content | output | editor | diagnostics published per document; the tail is summarized in one row |
| `nml-lsp::server::MAX_INDEX_BYTES` | 16 MiB | content | memory | editor | bytes of one workspace file the editor holds, indexed or open; past it the file is not indexed (said) and an open buffer is refused with one row |
| `nml-lsp::server::MAX_LOCATE_BYTES` | 8 MiB | content | memory | editor | bytes read when locating a related note's file |
| `nml-lsp::server::MAX_SUGGESTION_ACTIONS` | 8 | content | output | editor | code actions minted from one diagnostic's suggestions |
| `nml-lsp::server::MAX_UNIVERSE_FILES` | 128 | content | memory | editor | files an uncovered universe loads for one diagnostics pull |
| `nml-lsp::transport::framing::MAX_FRAME_BYTES` | 256 MiB | peer | memory | editor | bytes of one JSON-RPC frame the wasm transport will allocate |
| `nml-lsp::transport::framing::MAX_HEADER_BYTES` | 8 KiB | peer | memory | editor | bytes of one JSON-RPC header line the wasm transport will read |
<!-- nml limits: end -->

`source` is the workspace KEY of the file — on a located finding, a
universe note and a `related[]` entry alike, however the target was
typed (`../ws/tenants/cu/x.flow.nml` and `x.flow.nml` from inside `cu`
are one `tenants/cu/x.flow.nml`); only the human `file:line:col` prefix
keeps the path as typed. A `--schema` directory's source is not a
workspace file and is named by its basename. `message` is the sanitized
rendering — control characters arrive escaped, never raw.

