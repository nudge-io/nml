#!/usr/bin/env python3
"""Docs verification harness (DOCUMENTATION-PLAN Phase 5).

Extracts fenced ```nml code blocks from the Markdown docs and runs the tagged
ones through the real `nml` CLI, so documentation examples cannot rot.

Tag grammar (the fence info string after the language word):

    ```nml check                        block must parse       (nml check)
    ```nml check schema=<dir>           block must validate    (nml check --schema <dir>)
    ```nml check strict                 adds --strict (unknowns become errors)
    ```nml check expect-error=<text>    block must FAIL and the output must
                                        contain <text> (spaces: use expect-error="a b")
    ```nml check expect-output=<text>   output must contain <text>, exit code
                                        free (for warning-severity examples)
    ```nml check eol=crlf|cr            re-transcribe the block's line endings
                                        before running (fences are stored LF)
                                        so line-ending claims are executable

Opt-in (v1): only blocks tagged `check` are verified; untagged blocks are
counted and reported so coverage is visible. Once the guides are rewritten
(plan Phase 4), set OPT_OUT = True below: verification becomes the default
and `fragment` becomes the only escape hatch:

    ```nml fragment                     never verified (illustrative excerpt)

Beyond fenced blocks, more passes run:

- Example files: every `spec/examples/*.nml` is checked with the real CLI —
  `*.model.nml` schema files via `nml validate`, instance files via
  `nml check --schema spec/examples` (models live in the same directory).
- Tutorial fixtures: every chapter directory under `docs/tutorial/examples/`
  gets the same treatment — models via `nml validate`, instance files via
  `nml check`, with `--schema <chapter dir>` once the chapter has a model.
  Each chapter's final config state is therefore CI-verified.
- Tutorial programs (`TUTORIAL_APPS`): the chapter app crates are workspace
  members; this script `cargo run`s each from its chapter directory and
  asserts the output the tutorial page claims — the pages' "what you'll see"
  is tested, not trusted.
- Rust source sync: a ```rust block tagged `source=<repo-rel-file>` must be a
  verbatim substring of that file, so a page's full-program listing cannot
  drift from the compiled crate.
- Banned legacy tokens: syntax the language has removed must not reappear in
  teaching material. Enforced inside nml blocks and example files only (raw
  prose and Rust snippets legitimately contain e.g. `=>`), skipping
  `expect-error` blocks (deliberate demonstrations) and historical records
  (`docs/rfcs/`, the plan document), which are supposed to describe the old
  world.

The `nml` binary is taken from $NML_BIN, else target/debug/nml (build with
`cargo build -p nml-cli` first — the `just docs-test` recipe does).

Other Rust snippets are NOT handled here: crate/module doc examples are
doc-tests (`cargo test`).
"""

from __future__ import annotations

import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

OPT_OUT = False  # flip when the guide rewrite lands (see module docstring)

REPO = Path(__file__).resolve().parent.parent

DOC_GLOBS = [
    "README.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "docs/**/*.md",
    "spec/**/*.md",
    "crates/*/README.md",
    "crates/nml-core/assets/*.md",
    "nml-cli/README.md",
]

FENCE_RE = re.compile(r"^```nml\b(.*)$")

EXAMPLE_DIR = "spec/examples"
TUTORIAL_DIR = "docs/tutorial/examples"

# Tutorial chapter programs: (workspace package, chapter dir, expected output
# substring). Run from the chapter directory so relative config paths in the
# teaching code resolve. Entries are added as their chapters land; the crates
# are workspace members, so `cargo test/clippy/fmt` cover them too.
TUTORIAL_APPS: list[tuple[str, str, str]] = [
    ("nml-tutorial-07", "docs/tutorial/examples/07", "4 endpoint(s)"),
    ("nml-tutorial-08", "docs/tutorial/examples/08", "restart required"),
    ("nml-tutorial-09", "docs/tutorial/examples/09", "store has: skylight v0.1.0"),
]

RUST_FENCE_RE = re.compile(r"^```rust\s+(\S.*)$")

# Removed syntax that must never be re-taught. `=>` is banned only inside nml
# blocks and example files (Rust match arms in prose legitimately use it);
# `<shorthand>` is additionally banned in teaching prose — it has no
# legitimate non-historical use anywhere.
BANNED_TOKENS = ["<shorthand>", "=>"]
BANNED_PROSE_TOKENS = ["<shorthand>"]

# Removed syntax with a shape a bare substring can't pin — same scope as
# BANNED_TOKENS (nml blocks and example files). Each pattern names a dead
# form so the failure message teaches the living replacement's existence:
#   - angle-bracket constraints  → typed fields / set<T> / schema validation
#   - `model x (trait):`         → `model x is trait:`
#   - `&Type` reference marker   → plain references (conjunction atoms
#     start with `@`, so `&[A-Za-z]` can never hit a legal conjunction)
#   - `[]@roleRef` element type  → `[]role`
#   - `duration = "…"` default   → the duration literal (RFC 0017: `= 30s`).
#     Only the field-DEFINITION shape is pinnable by pattern (a quoted
#     string in an untyped instance property is legal string data), but a
#     `duration`-typed default with a quoted value is unambiguous.
BANNED_PATTERNS: list[tuple[str, "re.Pattern[str]"]] = [
    (
        "angle-bracket constraint",
        re.compile(r"<(unique|token|distinct|integer|secret)>|<(min|max|minLength|maxLength|pattern|currency)\s*="),
    ),
    ("parenthesized composition", re.compile(r"^(?:model|trait)\s+\w+\s*\(", re.M)),
    ("'&'-reference marker", re.compile(r"&[A-Za-z]")),
    ("'[]@' element type", re.compile(r"\[\]@")),
    ("quoted duration default", re.compile(r"\bduration\??\s*=\s*\"")),
]

# Historical records may (and should) describe removed syntax.
BAN_EXEMPT_PATHS = ("docs/rfcs/", "docs/DOCUMENTATION-PLAN.md")


def nml_bin() -> Path:
    env = os.environ.get("NML_BIN")
    if env:
        return Path(env)
    exe = "nml.exe" if os.name == "nt" else "nml"
    return REPO / "target" / "debug" / exe


def doc_files() -> list[Path]:
    seen: dict[Path, None] = {}
    for pattern in DOC_GLOBS:
        for p in sorted(REPO.glob(pattern)):
            if p.is_file():
                seen[p] = None
    return list(seen)


class Block:
    def __init__(self, path: Path, line: int, info: str, text: str):
        self.path = path
        self.line = line  # 1-based line of the opening fence
        self.text = text
        try:
            self.tags = shlex.split(info)
        except ValueError as e:
            # A malformed info string (e.g. an unbalanced quote) is a docs
            # bug; surface it as a check failure, not a traceback.
            self.tags = ["check"]
            self.malformed = f"malformed fence info string {info!r}: {e}"
            return
        self.malformed = None

    def has(self, name: str) -> bool:
        return name in self.tags

    def value(self, name: str) -> str | None:
        prefix = name + "="
        for t in self.tags:
            if t.startswith(prefix):
                return t[len(prefix):]
        return None

    @property
    def checked(self) -> bool:
        if self.has("fragment"):
            return False
        return OPT_OUT or self.has("check")

    def where(self) -> str:
        return f"{self.path.relative_to(REPO).as_posix()}:{self.line}"


def extract_blocks(path: Path) -> list[Block]:
    blocks: list[Block] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    i = 0
    while i < len(lines):
        m = FENCE_RE.match(lines[i])
        if m:
            start = i + 1  # 1-based fence line
            body: list[str] = []
            i += 1
            while i < len(lines) and lines[i].strip() != "```":
                body.append(lines[i])
                i += 1
            blocks.append(Block(path, start, m.group(1).strip(), "\n".join(body) + "\n"))
        i += 1
    return blocks


def run_check(block: Block) -> tuple[bool, str]:
    """Returns (passed, detail)."""
    if block.malformed:
        return False, block.malformed
    with tempfile.TemporaryDirectory() as td:
        sample = Path(td) / "example.nml"
        text = block.text
        eol = block.value("eol")
        if eol is not None:
            # Fences are stored LF (the repo pins LF at checkout); `eol=`
            # re-transcribes the snippet before it runs, so line-ending
            # claims are executable, not prose: `crlf` proves Windows
            # transcriptions mean the same document, `cr` demonstrates the
            # bare-CR diagnostic (NML0016).
            if eol == "crlf":
                text = text.replace("\n", "\r\n")
            elif eol == "cr":
                text = text.replace("\n", "\r")
            else:
                return False, f"unknown eol= value {eol!r} (expected crlf or cr)"
        # Bytes, not text mode: the transcription must reach the file exactly.
        sample.write_bytes(text.encode("utf-8"))
        cmd = [str(nml_bin()), "check"]
        if block.has("strict"):
            cmd.append("--strict")
        schema = block.value("schema")
        if schema:
            schema_dir = (REPO / schema).resolve()
            # Schema paths are repo-relative by contract; anything that
            # escapes the repo is a docs bug (and would make the check
            # depend on machine state outside the checkout).
            if not schema_dir.is_relative_to(REPO):
                return False, f"schema dir escapes the repository: {schema}"
            if not schema_dir.is_dir():
                return False, f"schema dir not found: {schema}"
            cmd += ["--schema", str(schema_dir)]
        cmd.append(str(sample))
        try:
            # Per-block timeout: one hanging example must fail fast, not eat
            # the CI job's whole budget.
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
        except subprocess.TimeoutExpired:
            return False, "nml check did not finish within 60s"
        output = proc.stdout + proc.stderr

        expected_error = block.value("expect-error")
        if expected_error is not None:
            if proc.returncode == 0:
                return False, "expected an error, but the check passed"
            if expected_error not in output:
                return False, (
                    f"error output did not contain {expected_error!r};"
                    f" got:\n{output.strip()}"
                )
            return True, ""
        expected_output = block.value("expect-output")
        if expected_output is not None:
            # Exit-code-free: warnings don't fail `nml check`, but the
            # documented output must still appear.
            if expected_output not in output:
                return False, (
                    f"output did not contain {expected_output!r};"
                    f" got:\n{output.strip()}"
                )
            return True, ""
        if proc.returncode != 0:
            return False, output.strip()
        return True, ""


def banned_tokens_in(text: str) -> list[str]:
    hits = [tok for tok in BANNED_TOKENS if tok in text]
    hits.extend(name for name, pat in BANNED_PATTERNS if pat.search(text))
    return hits


def run_cmd(
    cmd: list[str],
    timeout: int = 60,
    cwd: Path | None = None,
    stdin: int | None = None,
) -> tuple[int | None, str]:
    """Run a subprocess; returns (returncode, combined output). A timeout
    yields (None, <message>) so one hanging example fails fast instead of
    eating the CI job's whole budget."""
    # CARGO_TARGET_DIR is dropped to match the justfile recipes: everything
    # this script builds or runs lands in the repo's own target/.
    env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd,
            env=env,
            stdin=stdin,
        )
    except subprocess.TimeoutExpired as expired:
        # Cargo takes an EXCLUSIVE lock on the build directory, so any other
        # cargo on the machine (a parallel `cargo test`, rust-analyzer's
        # `cargo check`) makes this invocation wait rather than run. That
        # wait counts against the wall-clock timeout, so a step that does
        # seconds of real work can trip it — reported as "did not finish",
        # which reads as a hang and sends the reader hunting a defect that
        # is not there. Name the real cause when cargo told us about it.
        partial = "".join(
            part for part in (expired.stdout, expired.stderr) if isinstance(part, str)
        )
        if "waiting for file lock" in partial:
            return None, (
                f"{cmd[0]} spent its whole {timeout}s budget blocked on cargo's "
                "build-directory lock — another cargo (parallel test run, "
                "rust-analyzer) held it. This step did not hang; re-run with "
                "the workspace idle."
            )
        return None, f"{cmd[0]} did not finish within {timeout}s"
    return proc.returncode, proc.stdout + proc.stderr


def check_example_files() -> tuple[int, int, list[tuple[str, str]]]:
    """Check every spec/examples/*.nml with the real CLI. Returns
    (checked, passed, failures) where failures are (where, detail)."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    example_dir = REPO / EXAMPLE_DIR
    for path in sorted(example_dir.glob("*.nml")):
        checked += 1
        where = path.relative_to(REPO).as_posix()
        if bad := banned_tokens_in(path.read_text(encoding="utf-8")):
            failures.append((where, f"banned legacy token(s): {', '.join(bad)}"))
            continue
        if path.name.endswith(".model.nml"):
            cmd = [str(nml_bin()), "validate", str(path)]
        else:
            cmd = [str(nml_bin()), "check", "--schema", str(example_dir), str(path)]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
        except subprocess.TimeoutExpired:
            failures.append((where, "nml did not finish within 60s"))
            continue
        if proc.returncode != 0:
            failures.append((where, (proc.stdout + proc.stderr).strip()))
        else:
            passed += 1
    # No silent caps: an empty result means the examples moved or were
    # deleted, not that everything passed.
    if checked == 0:
        failures.append(
            (EXAMPLE_DIR, "no example files found — restore them or update EXAMPLE_DIR")
        )
    return checked, passed, failures


def check_tutorial_files() -> tuple[int, int, list[tuple[str, str]]]:
    """Check every docs/tutorial/examples/<chapter>/*.nml with the real CLI.
    Models are validated; instance files are checked against the chapter's
    directory once it contains a model (the chapters before schemas exist
    parse-check only). Returns (checked, passed, failures)."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    root = REPO / TUTORIAL_DIR
    chapters = sorted(p for p in root.iterdir() if p.is_dir()) if root.is_dir() else []
    # No silent caps: an empty result means the tutorial moved or was
    # deleted, not that everything passed.
    if not chapters:
        return (
            0,
            0,
            [(TUTORIAL_DIR, "no tutorial chapters found — restore them or update TUTORIAL_DIR")],
        )
    for chapter in chapters:
        files = sorted(chapter.glob("*.nml"))
        if not files:
            failures.append(
                (chapter.relative_to(REPO).as_posix(), "chapter has no .nml fixtures")
            )
            continue
        has_schema = any(f.name.endswith(".model.nml") for f in files)
        for path in files:
            checked += 1
            where = path.relative_to(REPO).as_posix()
            if bad := banned_tokens_in(path.read_text(encoding="utf-8")):
                failures.append((where, f"banned legacy token(s): {', '.join(bad)}"))
                continue
            if path.name.endswith(".model.nml"):
                cmd = [str(nml_bin()), "validate", str(path)]
            elif has_schema:
                cmd = [str(nml_bin()), "check", "--schema", str(chapter), str(path)]
            else:
                cmd = [str(nml_bin()), "check", str(path)]
            code, output = run_cmd(cmd)
            if code != 0:
                failures.append((where, output.strip()))
            else:
                passed += 1
    return checked, passed, failures


COOKBOOK_DIR = "docs/guides/examples/cookbook"


def run_cookbook() -> tuple[int, int, list[tuple[str, str]]]:
    """Run EVERY cookbook example (auto-enumerated — a new recipe can't be
    forgotten) plus the crate's tests (the TOML-equivalence and schema-test
    recipes). Each example must print `recipe OK`; stdin is closed so the
    embed-lsp recipe's server exits on EOF. Returns (checked, passed,
    failures).

    **One build, then plain binaries.** Cargo holds an exclusive lock on the
    build directory for the whole of every invocation, so a `cargo run` per
    recipe meant a dozen lock acquisitions that serialize behind any other
    cargo on the machine — turning a seconds-long check into a 300s timeout
    that looked like a hung recipe (it was not; it never got the lock).
    Building once and executing the produced binaries directly takes cargo
    out of the hot loop: one lock acquisition total, no per-recipe cargo
    overhead, and each recipe's timeout finally measures the recipe rather
    than the machine's build queue. Executable paths come from cargo's own
    JSON artifact messages, so nothing here guesses at target layout."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    examples_dir = REPO / COOKBOOK_DIR / "examples"
    examples = sorted(p.stem for p in examples_dir.glob("*.rs"))
    if not examples:
        return 1, 0, [(COOKBOOK_DIR, "no cookbook examples found — wiring broken?")]

    code, output = run_cmd(
        [
            "cargo", "build", "-p", "nml-cookbook", "--examples",
            "--message-format=json",
        ],
        timeout=600,
    )
    if code != 0:
        # One build failure is one finding, not a dozen identical ones.
        return len(examples) + 1, 0, [("cookbook:build", output.strip())]
    binaries: dict[str, str] = {}
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = msg.get("target") or {}
        if msg.get("executable") and "example" in (target.get("kind") or []):
            binaries[target.get("name", "")] = msg["executable"]

    for name in examples:
        checked += 1
        exe = binaries.get(name)
        if exe is None:
            failures.append(
                (f"cookbook:{name}", "cargo built no executable for this example")
            )
            continue
        code, output = run_cmd([exe], timeout=120, stdin=subprocess.DEVNULL)
        if code != 0:
            failures.append((f"cookbook:{name}", output.strip()))
        elif "recipe OK" not in output:
            failures.append((f"cookbook:{name}", f"missing 'recipe OK' marker; got:\n{output.strip()}"))
        else:
            passed += 1
    checked += 1
    code, output = run_cmd(
        ["cargo", "test", "--quiet", "-p", "nml-cookbook"], timeout=300
    )
    if code != 0:
        failures.append(("cookbook:tests", output.strip()))
    else:
        passed += 1
    return checked, passed, failures


def run_tutorial_apps() -> tuple[int, int, list[tuple[str, str]]]:
    """Compile AND run each tutorial chapter program, asserting the output its
    page claims. Returns (checked, passed, failures)."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    for package, chapter, expect in TUTORIAL_APPS:
        checked += 1
        # Generous timeout: the first run compiles the crate (CI shares the
        # workspace target dir with the nml-cli build, so deps are warm).
        code, output = run_cmd(
            ["cargo", "run", "--quiet", "-p", package],
            timeout=300,
            cwd=REPO / chapter,
        )
        if code != 0:
            failures.append((package, output.strip()))
        elif expect not in output:
            failures.append(
                (package, f"output did not contain {expect!r}; got:\n{output.strip()}")
            )
        else:
            passed += 1
    return checked, passed, failures


def check_rust_source_blocks(path: Path) -> tuple[int, list[tuple[str, str]]]:
    """```rust source=<repo-rel-file> blocks must be a verbatim substring of
    that file — the page's program listing cannot drift from the compiled
    crate. Returns (checked, failures)."""
    checked = 0
    failures: list[tuple[str, str]] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    i = 0
    while i < len(lines):
        m = RUST_FENCE_RE.match(lines[i])
        if m is None:
            i += 1
            continue
        where = f"{path.relative_to(REPO).as_posix()}:{i + 1}"
        body: list[str] = []
        i += 1
        while i < len(lines) and lines[i].strip() != "```":
            body.append(lines[i])
            i += 1
        i += 1
        try:
            tags = shlex.split(m.group(1))
        except ValueError as e:
            failures.append((where, f"malformed fence info string: {e}"))
            continue
        source = next(
            (t[len("source="):] for t in tags if t.startswith("source=")), None
        )
        if source is None:
            continue
        checked += 1
        source_path = (REPO / source).resolve()
        if not source_path.is_relative_to(REPO):
            failures.append((where, f"source path escapes the repository: {source}"))
        elif not source_path.is_file():
            failures.append((where, f"source file not found: {source}"))
        elif "\n".join(body) not in source_path.read_text(encoding="utf-8"):
            failures.append(
                (where, f"block is not a verbatim excerpt of {source} — resync them")
            )
        continue
    return checked, failures


ERROR_INDEX = "crates/nml-core/assets/error-index.md"
CODES_SOURCE = "crates/nml-core/src/diagnostic.rs"


def check_error_index() -> list[tuple[str, str]]:
    """Bidirectional drift guard: every code constant has a `## NML####`
    section in the error index, and every section corresponds to a declared
    constant. A new code cannot ship without its documentation (and a page
    cannot outlive its code without a visible failure).

    Also enforces **ascending section order** — the reader-facing half of
    the rule `diagnostic.rs` enforces at compile time for the declarations
    themselves. This index is a lookup table: someone who hit `NML3001`
    scans for it, and a section filed after `NML3003` is a section they
    walk past. Ordering is the affordance that makes scanning work, so it
    is checked rather than hoped for."""
    codes_text = (REPO / CODES_SOURCE).read_text(encoding="utf-8")
    declared = {
        f"NML{int(m):04}" for m in re.findall(r"^\s+[A-Z_]+ = (\d+);", codes_text, re.M)
    }
    index_path = REPO / ERROR_INDEX
    if not index_path.is_file():
        return [(ERROR_INDEX, "error index missing — every code needs a section")]
    index_text = index_path.read_text(encoding="utf-8")
    order = [int(m) for m in re.findall(r"^## NML(\d{4})", index_text, re.M)]
    documented = {f"NML{n:04}" for n in order}
    failures = []
    if undocumented := sorted(declared - documented):
        failures.append((ERROR_INDEX, f"codes missing a section: {', '.join(undocumented)}"))
    if orphaned := sorted(documented - declared):
        failures.append((ERROR_INDEX, f"sections for undeclared codes: {', '.join(orphaned)}"))
    # The band table is stated twice — once as rustdoc for library
    # consumers, once in the index preamble for people reading the error
    # pages. Both are the right audience for it, so the duplication stays;
    # what does not stay is the drift (RFC 0017 widened band 3000 to
    # "durations" in the rustdoc and the index kept saying "values &
    # money" for a release cycle). Compare the parsed band→label maps, so
    # wording and line-wrapping may differ but the meaning cannot.
    def bands(text: str, anchor: str) -> dict[str, str] | None:
        """The band→label map under `anchor`, or `None` when the anchor is
        gone. `None` is reported as an ordinary failure below rather than
        crashing on `.group()`: a guard whose own footing moved must say so
        in the language of the other findings, not as a traceback that
        sends the reader hunting the wrong defect."""
        located = re.search(anchor, text, re.S | re.M)
        if located is None:
            return None
        # Collapse to one line and drop rustdoc continuation markers, so a
        # doc comment and a markdown paragraph normalize identically.
        flat = " ".join(located.group(0).replace("///", " ").split())
        return {
            lo: " ".join(label.split())
            for lo, label in re.findall(r"(\d{4})[–-]\d{4} ([^·.]+)", flat)
        }

    code_bands = bands(codes_text, r"The stable code space.*?editor/LSP")
    index_bands = bands(index_text, r"^Bands \(.*?editor/LSP")
    if code_bands is None or index_bands is None:
        for where, table in ((CODES_SOURCE, code_bands), (ERROR_INDEX, index_bands)):
            if table is None:
                failures.append(
                    (
                        where,
                        "band table not found — either it was removed (restore "
                        "it: both audiences need it) or its wording moved and "
                        "`check_error_index`'s anchor needs updating",
                    )
                )
    elif code_bands != index_bands:
        differing = sorted(
            b for b in set(code_bands) | set(index_bands)
            if code_bands.get(b) != index_bands.get(b)
        )
        failures.append(
            (
                ERROR_INDEX,
                "band table disagrees with the rustdoc in "
                f"{CODES_SOURCE} for band(s) {', '.join(differing)}: "
                f"{ {b: (code_bands.get(b), index_bands.get(b)) for b in differing} }",
            )
        )
    if misordered := [
        f"NML{order[i + 1]:04} after NML{order[i]:04}"
        for i in range(len(order) - 1)
        if order[i] > order[i + 1]
    ]:
        failures.append(
            (
                ERROR_INDEX,
                "sections must be in ascending code order (readers scan this "
                f"index by number): {', '.join(misordered)}",
            )
        )
    failures.extend(check_relative_links(index_path))
    return failures


def tracked_files() -> set[str]:
    """Repo-relative POSIX paths of every git-TRACKED file. Links are judged
    against this, not the local filesystem: a target that exists locally but
    is gitignored or uncommitted (docs/rfcs/, held governance files) is
    BROKEN in every clean checkout and on GitHub — exactly the failure CI
    sees and a local `exists()` check cannot. Falls back to empty (existence
    check only) if git is unavailable."""
    try:
        out = subprocess.run(
            ["git", "ls-files"], capture_output=True, text=True, cwd=REPO, timeout=30
        )
        if out.returncode == 0:
            return set(out.stdout.splitlines())
    except (OSError, subprocess.TimeoutExpired):
        pass
    return set()


TRACKED = tracked_files()


def check_relative_links(path: Path) -> list[tuple[str, str]]:
    """Every relative link in `path` must resolve to a git-TRACKED file (or
    a directory containing one) inside the repo, from the document's own
    directory. Existence alone is not enough — see [`tracked_files`]. The
    error index already moved home once, stranding links written for the
    old home; this guard makes both rot classes a visible failure. Fenced
    lines are code content, not prose — skipped."""
    failures = []
    where = path.relative_to(REPO).as_posix()
    in_fence = False
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        for target in re.findall(r"\]\(([^)]+)\)", line):
            if target.startswith(("http://", "https://", "#", "mailto:")):
                continue
            resolved = (path.parent / target.split("#")[0]).resolve()
            if not resolved.is_relative_to(REPO):
                failures.append(
                    (where, f"line {number}: link escapes the repo: {target}")
                )
                continue
            rel = resolved.relative_to(REPO).as_posix()
            ok = resolved.exists() and (
                not TRACKED
                or rel in TRACKED
                or any(t.startswith(rel + "/") for t in TRACKED)
            )
            if not ok:
                failures.append(
                    (
                        where,
                        f"line {number}: relative link does not resolve to a "
                        f"tracked file: {target}",
                    )
                )
    return failures


def check_guide_links() -> list[tuple[str, str]]:
    """The cookbook's pages link across the docs tree and into the example
    crate, and the proof-surface pages (case study, footprint) link into
    both; every one of those links must resolve. (Whole-tree link checking
    is the site build's job at plan Phase 5; these are covered now because
    they are new and link-dense.)"""
    failures = []
    pages = sorted((REPO / "docs/guides").glob("*.md"))
    pages += [
        REPO / "docs/case-study.md",
        REPO / "docs/footprint.md",
        # The front door and the release record: their links break loudest.
        REPO / "README.md",
        REPO / "CHANGELOG.md",
    ]
    for page in pages:
        failures.extend(check_relative_links(page))
    return failures


def main() -> int:
    binary = nml_bin()
    if not binary.exists():
        print(f"docs-test: `nml` binary not found at {binary}", file=sys.stderr)
        print("build it first: cargo build -p nml-cli", file=sys.stderr)
        return 2

    checked = passed = 0
    unverified = 0
    rust_synced = 0
    failures: list[tuple[str, str]] = []

    for path in doc_files():
        rust_checked, rust_failures = check_rust_source_blocks(path)
        rust_synced += rust_checked
        failures.extend(rust_failures)
        # as_posix: ban paths use forward slashes; a Windows checkout must
        # not un-exempt the RFCs (or exempt nothing) via backslash paths.
        rel = path.relative_to(REPO).as_posix()
        exempt = any(
            rel == p or (p.endswith("/") and rel.startswith(p)) for p in BAN_EXEMPT_PATHS
        )
        if not exempt:
            prose_bad = [
                tok
                for tok in BANNED_PROSE_TOKENS
                if tok in path.read_text(encoding="utf-8")
            ]
            if prose_bad:
                failures.append(
                    (rel, f"banned legacy token(s) in prose: {', '.join(prose_bad)}")
                )
        for block in extract_blocks(path):
            # Legacy-token ban: every nml block on a teaching surface, except
            # deliberate error demonstrations.
            if not exempt and block.value("expect-error") is None:
                if bad := banned_tokens_in(block.text):
                    failures.append(
                        (block.where(), f"banned legacy token(s): {', '.join(bad)}")
                    )
                    continue
            if not block.checked:
                unverified += 1
                continue
            checked += 1
            ok, detail = run_check(block)
            if ok:
                passed += 1
            else:
                failures.append((block.where(), detail))

    files_checked, files_passed, file_failures = check_example_files()
    failures.extend(file_failures)
    tut_checked, tut_passed, tut_failures = check_tutorial_files()
    failures.extend(tut_failures)
    apps_checked, apps_passed, app_failures = run_tutorial_apps()
    failures.extend(app_failures)
    cb_checked, cb_passed, cb_failures = run_cookbook()
    failures.extend(cb_failures)
    failures.extend(check_error_index())
    failures.extend(check_guide_links())

    for where, detail in failures:
        print(f"FAIL {where}")
        for line in detail.splitlines():
            print(f"     {line}")

    print(
        f"docs-test: {passed}/{checked} verified blocks passed,"
        f" {files_passed}/{files_checked} example files passed,"
        f" {tut_passed}/{tut_checked} tutorial fixtures passed,"
        f" {apps_passed}/{apps_checked} tutorial programs passed,"
        f" {cb_passed}/{cb_checked} cookbook recipes passed,"
        f" {rust_synced} rust listings source-synced,"
        f" {unverified} untagged/fragment blocks not verified"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
