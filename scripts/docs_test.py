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

Opt-in (v1): only blocks tagged `check` are verified; untagged blocks are
counted and reported so coverage is visible. Once the guides are rewritten
(plan Phase 4), set OPT_OUT = True below: verification becomes the default
and `fragment` becomes the only escape hatch:

    ```nml fragment                     never verified (illustrative excerpt)

Beyond fenced blocks, two more passes run:

- Example files: every `spec/examples/*.nml` is checked with the real CLI —
  `*.model.nml` schema files via `nml validate`, instance files via
  `nml check --schema spec/examples` (models live in the same directory).
- Banned legacy tokens: syntax the language has removed must not reappear in
  teaching material. Enforced inside nml blocks and example files only (raw
  prose and Rust snippets legitimately contain e.g. `=>`), skipping
  `expect-error` blocks (deliberate demonstrations) and historical records
  (`docs/rfcs/`, the plan document), which are supposed to describe the old
  world.

The `nml` binary is taken from $NML_BIN, else target/debug/nml (build with
`cargo build -p nml-cli` first — the `just docs-test` recipe does).

Rust snippets are NOT handled here: crate/module doc examples are doc-tests
(`cargo test`), and tutorial programs live in docs/tutorial/examples/*/ and
compile in CI.
"""

from __future__ import annotations

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

# Removed syntax that must never be re-taught. `=>` is banned only inside nml
# blocks and example files (Rust match arms in prose legitimately use it);
# `<shorthand>` is additionally banned in teaching prose — it has no
# legitimate non-historical use anywhere.
BANNED_TOKENS = ["<shorthand>", "=>"]
BANNED_PROSE_TOKENS = ["<shorthand>"]

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
        sample.write_text(block.text, encoding="utf-8")
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
    return [tok for tok in BANNED_TOKENS if tok in text]


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


ERROR_INDEX = "crates/nml-core/assets/error-index.md"
CODES_SOURCE = "crates/nml-core/src/diagnostic.rs"


def check_error_index() -> list[tuple[str, str]]:
    """Bidirectional drift guard: every code constant has a `## NML####`
    section in the error index, and every section corresponds to a declared
    constant. A new code cannot ship without its documentation (and a page
    cannot outlive its code without a visible failure)."""
    declared = {
        f"NML{int(m):04}"
        for m in re.findall(r"^\s+[A-Z_]+ = (\d+);", (REPO / CODES_SOURCE).read_text(encoding="utf-8"), re.M)
    }
    index_path = REPO / ERROR_INDEX
    if not index_path.is_file():
        return [(ERROR_INDEX, "error index missing — every code needs a section")]
    documented = set(re.findall(r"^## (NML\d{4})", index_path.read_text(encoding="utf-8"), re.M))
    failures = []
    if undocumented := sorted(declared - documented):
        failures.append((ERROR_INDEX, f"codes missing a section: {', '.join(undocumented)}"))
    if orphaned := sorted(documented - declared):
        failures.append((ERROR_INDEX, f"sections for undeclared codes: {', '.join(orphaned)}"))
    return failures


def main() -> int:
    binary = nml_bin()
    if not binary.exists():
        print(f"docs-test: `nml` binary not found at {binary}", file=sys.stderr)
        print("build it first: cargo build -p nml-cli", file=sys.stderr)
        return 2

    checked = passed = 0
    unverified = 0
    failures: list[tuple[str, str]] = []

    for path in doc_files():
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
    failures.extend(check_error_index())

    for where, detail in failures:
        print(f"FAIL {where}")
        for line in detail.splitlines():
            print(f"     {line}")

    print(
        f"docs-test: {passed}/{checked} verified blocks passed,"
        f" {files_passed}/{files_checked} example files passed,"
        f" {unverified} untagged/fragment blocks not verified"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
