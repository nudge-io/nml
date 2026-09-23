#!/usr/bin/env python3
"""Docs verification harness (DOCUMENTATION-PLAN Phase 5).

Extracts fenced ```nml code blocks from the Markdown docs and runs the tagged
ones through the real `nml` CLI, so documentation examples cannot rot.

Tag grammar (the fence info string after the language word):

    ```nml check                        block must parse       (nml check)
    ```nml check schema=<dir>           block must validate    (nml check --schema <dir>)
    ```nml check strict                 adds --strict (unknowns become errors)
    ```nml check expect-error=<text>    block must FAIL and the output must
                                        contain <text> (spaces: use expect-error="a b").
                                        Bracketed form = code-MULTISET equality:
                                        expect-error='[NML2057, NML2057]' declares
                                        EXACTLY two NML2057 findings — repetition is
                                        the count syntax, order ignored
    ```nml check expect-output=<text>   check must pass AND the output must
                                        contain <text> (warning examples); the
                                        bracketed form is the warning-side multiset
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
  members; this script builds them in one cargo invocation (see
  `cargo_binaries`) and runs each binary from its chapter directory,
  asserting the output the tutorial page claims — the pages' "what you'll
  see" is tested, not trusted.
- Rust source sync: a ```rust block tagged `source=<repo-rel-file>` must be a
  verbatim substring of that file, so a page's full-program listing cannot
  drift from the compiled crate.
- Banned legacy tokens: syntax the language has removed must not reappear in
  teaching material. Enforced inside nml blocks and example files only (raw
  prose and Rust snippets legitimately contain e.g. `=>`), skipping
  `expect-error` blocks (deliberate demonstrations) and the ban-exempt
  design records (`docs/rfcs/`, the plan document — they are supposed to
  describe the old world). The reserved-name scan below has NO
  exemption: it runs over every doc and every example file.

The `nml` binary is taken from $NML_BIN, else target/debug/nml (build with
`cargo build -p nml-cli` first — the `just gate-docs` recipe does).

Other Rust snippets are NOT handled here: crate/module doc examples are
doc-tests (`cargo test`).
"""

from __future__ import annotations

import functools
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
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

# The leading-whitespace group admits fences indented inside list items
# (tutorial <details> solutions); the captured prefix is stripped from the
# body before use — NML is indentation-sensitive, so the snippet must reach
# the CLI at column 0, exactly as a reader would save it.
FENCE_RE = re.compile(r"^([ \t]*)```nml\b(.*)$")

# A CLI transcript: a ```text fence tagged `transcript=<repo-relative dir>`
# is EXECUTED — every `$ nml …` line runs the real binary from that
# directory (stdout and stderr merged, as a terminal shows them) and the
# lines that follow it, up to the next `$` line, must be the output byte
# for byte (a `$ nml check …` example nobody runs drifts: every NML208x
# example once lacked the `for more information` line). Two more tags
# prepare a TEMP COPY of the fixture for what git cannot hold —
# `sparse=<path>:<bytes>` (a file of that many NUL bytes: the byte caps),
# `flood=<dir>:<count>` (that many empty files: the entry bound),
# `link=<path>:<target>` (a symlink, as git would check one out) and
# `fifo=<path>` (a named pipe: the non-regular-file refusals) —
# and the expected lines may spell the run's canonical root as `${ROOT}`,
# the binary's version as `${VERSION}` and the wire revision as
# `"revision":${REVISION}` (a literal revision in a transcript is refused:
# a revision bump is one constant, never a docs-wide edit), so a row that prints any
# is still compared byte for byte.
TRANSCRIPT_RE = re.compile(r"^([ \t]*)```text\s+transcript=(\S+)((?:\s+(?:sparse|flood|link|fifo)=\S+)*)\s*$")

# The `--json` wire contract (docs/stability.md): every row a transcript's
# `nml … --json` command prints is validated against this JSON Schema —
# every row type closed, every vocabulary enumerated, the revision a const
# on the opening `contract` row and the closing `summary` — so a field the
# schema does not know, a value outside its vocabulary, or a stream that
# does not open with the contract row, fails the docs gate the day it ships. The validator below is the small subset of JSON
# Schema the file uses (no third-party dependency).
NDJSON_SCHEMA_PATH = "docs/json/nml-ndjson-v1.schema.json"

EXAMPLE_DIR = "spec/examples"
TUTORIAL_DIR = "docs/tutorial/examples"

# Tutorial chapter programs: (workspace package, chapter dir, expected output
# substring). Run from the chapter directory so relative config paths in the
# teaching code resolve. Entries are added as their chapters land; the crates
# are workspace members, so `cargo test/clippy/fmt` cover them too.
TUTORIAL_APPS: list[tuple[str, str, str]] = [
    ("nml-tutorial-07", "docs/tutorial/examples/07", "4 endpoint(s)"),
    ("nml-tutorial-08", "docs/tutorial/examples/08", "restart required"),
    ("nml-tutorial-09", "docs/tutorial/examples/09", "store has: skylight v0.1.0 (de541008, "),
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

# The reserved-name rule (RFC 0023 Part F): first-party names — the
# directories of this workspace — are allowed in documentation; anything
# else is replaced by the documentation's fictional vocabulary. These two
# external identifiers appeared in early RFC examples, and this regex is
# THE ONLY place in the repository that may spell them: no document, the
# RFCs included, may quote them. Scanned WITHOUT the ban exemption, over
# every doc's full text and every example/tutorial `.nml` file. The
# leading word boundary has no trailing twin, so compound identifiers
# cannot smuggle a name back in.
RESERVED_NAMES = re.compile(r"\b(corelation|keystone)", re.IGNORECASE)


def reserved_names_in(text: str) -> list[str]:
    """Every reserved-name hit in `text`, deduplicated and lowercased."""
    return sorted({m.group(1).lower() for m in RESERVED_NAMES.finditer(text)})


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
            indent, info = m.groups()
            start = i + 1  # 1-based fence line
            body: list[str] = []
            i += 1
            while i < len(lines) and lines[i].strip() != "```":
                body.append(lines[i])
                i += 1
            if indent:
                # Dedent by exactly the fence's own prefix; lines without it
                # (blank lines) pass through unchanged.
                body = [
                    line[len(indent):] if line.startswith(indent) else line
                    for line in body
                ]
            blocks.append(Block(path, start, info.strip(), "\n".join(body) + "\n"))
        i += 1
    return blocks


class Transcript:
    def __init__(self, path: Path, line: int, fixture: str, prepare: str, text: str):
        self.path = path
        self.line = line
        self.fixture = fixture
        # `sparse=<path>:<bytes>` / `flood=<dir>:<count>` /
        # `link=<path>:<target>` tags, in order.
        self.prepare = prepare.split()
        self.text = text

    def where(self) -> str:
        return f"{self.path.relative_to(REPO).as_posix()}:{self.line}"


def extract_transcripts(path: Path) -> list[Transcript]:
    """The ```text transcript=<dir> fences of one document (see TRANSCRIPT_RE)."""
    out: list[Transcript] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    i = 0
    while i < len(lines):
        m = TRANSCRIPT_RE.match(lines[i])
        if m:
            indent, fixture, prepare = m.groups()
            start = i + 1
            body: list[str] = []
            i += 1
            while i < len(lines) and lines[i].strip() != "```":
                line = lines[i]
                body.append(line[len(indent):] if indent and line.startswith(indent) else line)
                i += 1
            out.append(Transcript(path, start, fixture, prepare, "\n".join(body) + "\n"))
        i += 1
    return out


@functools.cache
def nml_version() -> str:
    """The version the binary prints (`nml version`), for `${VERSION}`."""
    out = subprocess.run([str(nml_bin()), "version"], stdout=subprocess.PIPE, check=False)
    return out.stdout.decode("utf-8", errors="replace").strip().removeprefix("nml ")


@functools.cache
def nml_revision() -> str:
    """The wire revision the binary prints (`nml version --json`, the contract
    row — nml-cli/src/out.rs REVISION, the one source `check_wire_revision`
    holds the schema to), for `"revision":${REVISION}`. Absent (an older or
    broken binary), the placeholder stays unsubstituted and every `--json`
    transcript fails loudly rather than matching a guessed number."""
    out = subprocess.run(
        [str(nml_bin()), "version", "--json"], stdout=subprocess.PIPE, check=False
    )
    try:
        head = json.loads(out.stdout.decode("utf-8", errors="replace").splitlines()[0])
    except (IndexError, json.JSONDecodeError):
        return "${REVISION}"
    revision = head.get("revision") if head.get("type") == "contract" else None
    return str(revision) if isinstance(revision, int) else "${REVISION}"


# A transcript spells the wire revision through the placeholder only.
LITERAL_REVISION_RE = re.compile(r'"revision":\d+')


def prose_revision_faults(text: str, revision: int) -> list[tuple[int, str]]:
    """Every `"revision":N` in `text` that names a revision other than
    `revision`, as (1-based line, the spelling).

    A transcript cannot carry a literal at all — `run_transcript` refuses
    one and the runner substitutes `${REVISION}` — so a literal in a
    tracked document is PROSE: a line a reader learns the contract from,
    substituted by nobody and executed by nobody. Both lines that print
    the contract row to a reader sat a revision behind the writer."""
    faults: list[tuple[int, str]] = []
    for number, line in enumerate(text.splitlines(), 1):
        for m in LITERAL_REVISION_RE.finditer(line):
            if int(m.group().split(":")[1]) != revision:
                faults.append((number, m.group()))
    return faults


# What a transcript's preparation may ask of the machine running the docs
# gate: enough for every bound the kernel publishes (a flood past the
# 1,048,576-entry universe backstop, a file past the 16 MiB target cap and
# the 64 MiB live-input budget), never a billion inodes or a terabyte
# truncate from one line of a docs PR.
MAX_FLOOD_FILES = 2 * 1_048_576
MAX_SPARSE_BYTES = 64 * 1024 * 1024


def preparation(op: str) -> tuple[str, str, int | str]:
    """One parsed, cap-checked transcript preparation — `(kind, path,
    size | count | link target)` — PURE: nothing on disk is consulted or
    touched, so the caps are proven without the thing they cap (a
    loosened flood cap once put two million files on a developer's disk
    from the self-test's own over-cap ask). An over-cap or unknown ask
    is a ValueError."""
    kind, spec = op.split("=", 1)
    rel, _, number = spec.rpartition(":")
    if kind == "sparse":
        size = int(number)
        if size > MAX_SPARSE_BYTES:
            raise ValueError(f"{op}: a sparse file is at most {MAX_SPARSE_BYTES} bytes")
        return kind, rel, size
    if kind == "flood":
        count = int(number)
        if count > MAX_FLOOD_FILES:
            raise ValueError(f"{op}: a flood is at most {MAX_FLOOD_FILES} files")
        return kind, rel, count
    if kind == "link":
        return kind, rel, number
    if kind == "fifo":
        return kind, spec, 0
    raise ValueError(f"unknown transcript preparation: {op}")


def prepare_copy(fixture: Path, ops: list[str], into: Path) -> Path:
    """A temp copy of `fixture` with the transcript's preparation applied:
    `sparse=<path>:<bytes>` truncates (creates) a file to that size (at
    most MAX_SPARSE_BYTES), `flood=<dir>:<count>` fills a directory with
    that many empty files (at most MAX_FLOOD_FILES), `link=<path>:<target>`
    plants a symlink (the target as written, never resolved — a dangling
    one is a fine fixture), `fifo=<path>` makes a named pipe (the one
    non-regular file git cannot hold). Every op is planned through `preparation`
    BEFORE the fixture is copied: an over-cap ask is a ValueError that
    leaves `into` empty, which the runner reports as "cannot prepare the
    transcript's copy"."""
    plan = [preparation(op) for op in ops]
    copy = (into / fixture.name).resolve()
    shutil.copytree(fixture, copy, symlinks=True)
    for op, (kind, rel, value) in zip(ops, plan):
        target = (copy / rel).resolve()
        if not target.is_relative_to(copy):
            raise ValueError(f"{op}: escapes the fixture")
        if kind == "sparse":
            target.parent.mkdir(parents=True, exist_ok=True)
            with open(target, "wb") as f:
                f.truncate(value)
        elif kind == "flood":
            target.mkdir(parents=True, exist_ok=True)
            for n in range(value):
                (target / f"s{n}.txt").touch()
        elif kind == "fifo":
            target.parent.mkdir(parents=True, exist_ok=True)
            os.mkfifo(target)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.symlink_to(value)
    return copy


class SchemaViolation(Exception):
    """A JSON value does not conform: `path` locates it in the instance."""

    def __init__(self, path: str, why: str):
        super().__init__(f"{path or '$'}: {why}")


def load_ndjson_schema() -> dict:
    return json.loads((REPO / NDJSON_SCHEMA_PATH).read_text(encoding="utf-8"))


NDJSON_SCHEMA = load_ndjson_schema()
# The contract's two numbers as the schema states them, ONE site each
# (`$defs/formatVersion`, `$defs/revision` — the `contract` and `summary`
# rows reference them). The writer (`nml-cli/src/out.rs`) is the source;
# `check_wire_revision` holds the schema, the shape record and the
# CHANGELOG ledger to it.
SCHEMA_FORMAT_VERSION: int = NDJSON_SCHEMA["$defs"]["formatVersion"]["const"]
SCHEMA_REVISION: int = NDJSON_SCHEMA["$defs"]["revision"]["const"]


def _resolve_ref(root: dict, ref: str) -> dict:
    if not ref.startswith("#/"):
        raise ValueError(f"unsupported $ref: {ref}")
    node: object = root
    for part in ref[2:].split("/"):
        node = node[part]  # type: ignore[index]
    return node  # type: ignore[return-value]


def _type_matches(value: object, kind: str) -> bool:
    if kind == "null":
        return value is None
    if kind == "boolean":
        return isinstance(value, bool)
    if kind == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    if kind == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    if kind == "string":
        return isinstance(value, str)
    if kind == "array":
        return isinstance(value, list)
    if kind == "object":
        return isinstance(value, dict)
    raise ValueError(f"unsupported type keyword: {kind}")


def validate_json(value: object, schema: dict, root: dict, path: str = "") -> None:
    """Validate `value` against `schema` (a JSON Schema subset: $ref, type,
    const, enum, minimum, pattern, properties, required,
    additionalProperties, propertyNames, items, oneOf, anyOf); raise
    SchemaViolation at the first mismatch."""
    if "$ref" in schema:
        validate_json(value, _resolve_ref(root, schema["$ref"]), root, path)
        return
    if "type" in schema:
        kinds = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        if not any(_type_matches(value, k) for k in kinds):
            raise SchemaViolation(path, f"expected {' or '.join(kinds)}, got {json.dumps(value)[:80]}")
    if "const" in schema and value != schema["const"]:
        raise SchemaViolation(path, f"expected {json.dumps(schema['const'])}, got {json.dumps(value)[:80]}")
    if "enum" in schema and value not in schema["enum"]:
        raise SchemaViolation(path, f"{json.dumps(value)[:80]} is not one of {json.dumps(schema['enum'])}")
    if "minimum" in schema and isinstance(value, (int, float)) and not isinstance(value, bool):
        if value < schema["minimum"]:
            raise SchemaViolation(path, f"{value} is below the minimum {schema['minimum']}")
    if "pattern" in schema and isinstance(value, str) and not re.search(schema["pattern"], value):
        raise SchemaViolation(path, f"{value!r} does not match {schema['pattern']}")
    if isinstance(value, dict):
        props = schema.get("properties", {})
        for name in schema.get("required", []):
            if name not in value:
                raise SchemaViolation(path, f"missing required field {name!r}")
        for name, item in value.items():
            here = f"{path}.{name}" if path else name
            if "propertyNames" in schema:
                validate_json(name, schema["propertyNames"], root, here)
            if name in props:
                validate_json(item, props[name], root, here)
            elif "additionalProperties" in schema:
                extra = schema["additionalProperties"]
                if extra is False:
                    raise SchemaViolation(here, "a field the schema does not know")
                validate_json(item, extra, root, here)
    if isinstance(value, list) and "items" in schema:
        for i, item in enumerate(value):
            validate_json(item, schema["items"], root, f"{path}[{i}]")
    if "oneOf" in schema:
        matched = []
        failures = []
        for i, alternative in enumerate(schema["oneOf"]):
            try:
                validate_json(value, alternative, root, path)
                matched.append(i)
            except SchemaViolation as e:
                failures.append(str(e))
        if len(matched) != 1:
            raise SchemaViolation(
                path, f"{len(matched)} of {len(schema['oneOf'])} alternatives match; " + "; ".join(failures[:3])
            )
    if "anyOf" in schema:
        failures = []
        for alternative in schema["anyOf"]:
            try:
                validate_json(value, alternative, root, path)
                break
            except SchemaViolation as e:
                failures.append(str(e))
        else:
            raise SchemaViolation(path, "no alternative matches: " + "; ".join(failures[:3]))


def validate_ndjson_row(row: object, schema: dict) -> None:
    """One `--json` row against the contract, dispatched on `type` for a
    readable failure (the schema's top-level `oneOf` would otherwise
    report every row type's mismatch)."""
    if not isinstance(row, dict) or not isinstance(row.get("type"), str):
        raise SchemaViolation("", "a row is an object with a string `type`")
    for alternative in schema["oneOf"]:
        definition = _resolve_ref(schema, alternative["$ref"])
        if definition["properties"]["type"]["const"] == row["type"]:
            validate_json(row, definition, schema, row["type"])
            return
    raise SchemaViolation("type", f"unknown row type {row['type']!r}")


def ndjson_violations(output: str, schema: dict) -> tuple[int, str | None]:
    """Every line of a `--json` command's output must be one row of the
    contract, the first the `contract` row and the last the `summary`
    (the stream describes itself before and after everything it says):
    (rows validated, the first violation or None)."""
    rows = 0
    types: list[str] = []
    for n, line in enumerate(output.splitlines(), 1):
        if not line.strip():
            return rows, f"line {n} is blank (every line is one JSON object)"
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            return rows, f"line {n} is not JSON: {e}"
        try:
            validate_ndjson_row(row, schema)
        except SchemaViolation as e:
            return rows, f"line {n} ({line[:120]}): {e}"
        rows += 1
        types.append(row["type"])
    if types and types[0] != "contract":
        return rows, f"line 1 is a {types[0]!r} row: every --json stream opens with the contract row"
    if types and types[-1] != "summary":
        return rows, f"the last row is {types[-1]!r}: every --json stream closes with the summary row"
    return rows, None


# The validator must BITE before it is trusted: a row with a field the
# schema does not know, a severity outside the vocabulary, a summary at a
# formatVersion or a revision other than the schema's, a contract row at
# another revision and an unknown row type are each refused; a
# well-formed summary row and a well-formed contract row pass; and a
# stream that does not open with the contract row is refused as a stream.
NDJSON_SELF_TEST_GOOD = {
    "type": "summary", "formatVersion": SCHEMA_FORMAT_VERSION, "revision": SCHEMA_REVISION,
    "nmlVersion": "0.1.0", "verb": "limits",
    "exit": 0, "targets": 0, "errors": 0, "warnings": 0, "root": None, "universe": None,
    "closure": None, "manifests": None, "truncatedUnits": None, "skipped": None, "withheld": None,
    "schemaSources": None,
}
NDJSON_SELF_TEST_CONTRACT = {
    "type": "contract", "formatVersion": SCHEMA_FORMAT_VERSION, "revision": SCHEMA_REVISION,
    "nmlVersion": "0.1.0",
}
NDJSON_SELF_TEST_BAD: list[tuple[dict, str]] = [
    ({**NDJSON_SELF_TEST_GOOD, "extra": 1}, "a field the schema does not know"),
    ({**NDJSON_SELF_TEST_GOOD, "formatVersion": SCHEMA_FORMAT_VERSION + 1},
     f"expected {SCHEMA_FORMAT_VERSION}"),
    ({**NDJSON_SELF_TEST_GOOD, "revision": SCHEMA_REVISION + 1}, f"expected {SCHEMA_REVISION}"),
    ({**NDJSON_SELF_TEST_CONTRACT, "revision": SCHEMA_REVISION + 1}, f"expected {SCHEMA_REVISION}"),
    ({**NDJSON_SELF_TEST_CONTRACT, "extra": 1}, "a field the schema does not know"),
    ({"type": "diagnostic", "source": "x", "line": None, "col": None, "severity": "fatal",
      "code": None, "message": "m", "related": [], "suggestions": []}, "is not one of"),
    # A suggestion of a kind the vocabulary does not know, and an edit
    # missing its lines, are each refused — the validator bites on the
    # edit contract, not only on the row's own fields.
    ({"type": "diagnostic", "source": "x", "line": 1, "col": 1, "severity": "error",
      "code": None, "message": "m", "related": [],
      "suggestions": [{"kind": "guess", "source": "m", "edits": []}]}, "is not one of"),
    ({"type": "diagnostic", "source": "x", "line": 1, "col": 1, "severity": "error",
      "code": None, "message": "m", "related": [],
      "suggestions": [{"kind": "insert", "source": "m",
                       "edits": [{"line": 1, "col": 1, "endLine": 1, "endCol": 1}]}]},
     "missing required field 'lines'"),
    ({"type": "row-from-the-future"}, "unknown row type"),
]


def ndjson_validator_self_test(schema: dict) -> str | None:
    """None when the validator behaves; else what it got wrong."""
    for good, what in ((NDJSON_SELF_TEST_GOOD, "summary"), (NDJSON_SELF_TEST_CONTRACT, "contract")):
        try:
            validate_ndjson_row(good, schema)
        except SchemaViolation as e:
            return f"a well-formed {what} row was refused: {e}"
    stream = json.dumps(NDJSON_SELF_TEST_CONTRACT) + "\n" + json.dumps(NDJSON_SELF_TEST_GOOD) + "\n"
    if ndjson_violations(stream, schema) != (2, None):
        return f"a well-formed stream was refused: {ndjson_violations(stream, schema)}"
    _, headless = ndjson_violations(json.dumps(NDJSON_SELF_TEST_GOOD) + "\n", schema)
    if headless is None or "opens with the contract row" not in headless:
        return f"a stream without the contract row was accepted: {headless!r}"
    _, tailless = ndjson_violations(json.dumps(NDJSON_SELF_TEST_CONTRACT) + "\n", schema)
    if tailless is None or "closes with the summary row" not in tailless:
        return f"a stream without the summary row was accepted: {tailless!r}"
    for row, why in NDJSON_SELF_TEST_BAD:
        try:
            validate_ndjson_row(row, schema)
        except SchemaViolation as e:
            if why not in str(e):
                return f"refused {json.dumps(row)[:80]} for the wrong reason: {e} (expected {why!r})"
            continue
        return f"accepted a row it must refuse: {json.dumps(row)[:80]}"
    return None


json_rows_validated = 0


def prepare_caps_self_test() -> str | None:
    """The preparation caps must bite: an over-cap `flood=` and `sparse=`
    are each refused by the pure parse — proven WITHOUT the disk, so a
    loosened cap can never flood the machine running this self-test —
    the copy consults the parse before it copies anything (shown with
    the sparse ask, whose worst case under any defect is one hole-only
    file), and an in-cap ask still prepares. None when they behave; else
    what went wrong."""
    for op, expect in [
        (f"flood=x:{MAX_FLOOD_FILES + 1}", "a flood is at most"),
        (f"sparse=y:{MAX_SPARSE_BYTES + 1}", "a sparse file is at most"),
    ]:
        try:
            preparation(op)
        except ValueError as e:
            if expect not in str(e):
                return f"{op} refused for the wrong reason: {e}"
            continue
        return f"{op} was not refused"
    with tempfile.TemporaryDirectory(prefix="nml-caps-") as scratch:
        fixture = Path(scratch) / "fixture"
        fixture.mkdir()
        into = Path(scratch) / "over"
        into.mkdir()
        try:
            prepare_copy(fixture, [f"sparse=y:{MAX_SPARSE_BYTES + 1}"], into)
        except ValueError:
            if any(into.iterdir()):
                return "an over-cap ask copied something before refusing"
        else:
            return "the copy did not consult the caps"
        into = Path(scratch) / "ok"
        into.mkdir()
        try:
            copy = prepare_copy(fixture, ["flood=x:3", f"sparse=y:{MAX_SPARSE_BYTES}"], into)
        except (ValueError, OSError) as e:
            return f"an in-cap preparation was refused: {e}"
        if len(list((copy / "x").iterdir())) != 3 or (copy / "y").stat().st_size != MAX_SPARSE_BYTES:
            return "an in-cap preparation did not prepare"
    return None


def run_transcript(t: Transcript) -> tuple[bool, str]:
    """Execute one transcript; (passed, detail). Each `$ nml …` line runs
    from the fixture directory with the built binary; its expected output
    is the block up to the next `$` line."""
    fixture = (REPO / t.fixture).resolve()
    if not fixture.is_relative_to(REPO) or not fixture.is_dir():
        return False, f"transcript fixture not found in the repository: {t.fixture}"
    with tempfile.TemporaryDirectory(prefix="nml-transcript-") as scratch:
        cwd = fixture
        if t.prepare:
            try:
                cwd = prepare_copy(fixture, t.prepare, Path(scratch))
            except (ValueError, OSError) as e:
                return False, f"cannot prepare the transcript's copy: {e}"
        return run_transcript_in(t, cwd.resolve())


def run_transcript_in(t: Transcript, cwd: Path) -> tuple[bool, str]:
    """Run `t`'s commands from `cwd`; `${ROOT}`, `${VERSION}` and
    `"revision":${REVISION}` in its expected lines are the run's canonical
    root, the binary's version and the wire revision it prints (the token
    carries its key, so the reverse mapping on a mismatch is exact too)."""
    placeholders = {
        "${ROOT}": str(cwd),
        "${VERSION}": nml_version(),
        '"revision":${REVISION}': f'"revision":{nml_revision()}',
    }
    if literal := LITERAL_REVISION_RE.search(t.text):
        return False, (
            f"the transcript spells the wire revision literally ({literal.group()}) —"
            ' spell it "revision":${REVISION}; the docs gate substitutes the revision the'
            " binary prints, so a revision bump is one constant, never a docs-wide edit"
        )
    lines = t.text.splitlines()
    commands: list[tuple[str, list[str]]] = []
    for line in lines:
        if line.startswith("$ "):
            commands.append((line[2:], []))
        elif commands:
            commands[-1][1].append(line)
        elif line.strip():
            return False, f"transcript text before the first `$ ` line: {line!r}"
    if not commands:
        return False, "a transcript needs at least one `$ nml …` line"
    for command, expected in commands:
        for key, value in placeholders.items():
            expected = [line.replace(key, value) for line in expected]
        try:
            argv = shlex.split(command)
        except ValueError as e:
            return False, f"cannot parse {command!r}: {e}"
        if not argv or argv[0] != "nml":
            return False, f"a transcript runs `nml` only, not {command!r}"
        argv[0] = str(nml_bin())
        try:
            # The transcripts show the sentences' Unicode spelling: a recorded
            # output is the one a reader with a UTF-8 terminal sees.
            proc = subprocess.run(
                argv,
                cwd=cwd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=60,
                env={**os.environ, "NML_UNICODE": "1"},
            )
        except subprocess.TimeoutExpired:
            return False, f"{command!r} did not finish within 60s"
        output = proc.stdout.decode("utf-8", errors="replace")
        # Every `--json` command's rows are the versioned contract.
        if "--json" in argv:
            global json_rows_validated
            rows, violation = ndjson_violations(output, NDJSON_SCHEMA)
            json_rows_validated += rows
            if violation is not None:
                return False, (
                    f"`$ {command}` (in {t.fixture}) printed a row outside the --json contract"
                    f" ({NDJSON_SCHEMA_PATH}): {violation}"
                )
        actual = output.splitlines()
        if actual != expected:
            want = "\n".join(expected)
            got = "\n".join(actual)
            for key, value in placeholders.items():
                got = got.replace(value, key)
            return False, (
                f"`$ {command}` (in {t.fixture}) printed something else than the page shows"
                f" — re-record the fence from the binary:\n--- page\n{want}\n--- binary\n{got}"
            )
    return True, ""


def sample_owns_line(line: str, sample: Path) -> bool:
    """True when a CLI row attributes a finding to the checked sample file.

    Fences run `nml check` on an absolute temp path, but the CLI prints the
    file's workspace key (e.g. `example.nml` when the universe root is the
    temp directory). Match either spelling so code-multiset contracts stay
    path-scoped without requiring the absolute argv path in output."""
    if str(sample) in line:
        return True
    return line.startswith(f"{sample.name}:")


def sample_codes(output: str, sample: Path, severity: str) -> Counter[str]:
    """The `severity[NML####]` codes on the SAMPLE's own output lines, as a
    MULTISET — one count per finding, so an example that regresses from
    demonstrating two same-code findings to one fails instead of collapsing
    into an equal set. Path-scoped so a `schema=` dir's own findings are
    context, not part of the example's claim — the shared extraction behind
    both `expect-error` and `expect-output` code contracts."""
    return Counter(
        code
        for line in output.splitlines()
        if sample_owns_line(line, sample)
        for code in re.findall(rf"{severity}\[(NML\d{{4}})\]", line)
    )


def render_counts(counts: Counter[str]) -> str:
    """`NML2057×2, NML2058` — the multiset, human-readable, sorted."""
    return ", ".join(
        code if n == 1 else f"{code}×{n}" for code, n in sorted(counts.items())
    )


# The bracketed code-list spelling (`[NML2057]`, `[NML2057, NML2058]`) that
# selects code-set mode for an expectation.
CODE_SET_RE = re.compile(r"\[NML\d{4}(?:\s*,\s*NML\d{4})*\]")


def declared_codes(expectation: str) -> list[str]:
    """The codes of a code-set annotation, or [] for a prose expectation.
    Mode is gated on the bracketed spelling — prose that merely MENTIONS a
    code stays a text-containment claim."""
    if CODE_SET_RE.search(expectation):
        return re.findall(r"NML\d{4}", expectation)
    return []


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
            if not expected_error:
                # An empty expectation asserts nothing; prose containment
                # would degrade to always-true. A doc-author error, loudly.
                return False, "expect-error= is empty — declare a code list or message text"
            if proc.returncode == 0:
                return False, "expected an error, but the check passed"
            # Code-MULTISET equality, not containment and not a set: the
            # annotation declares the complete list of error codes the
            # EXAMPLE produces, counts included — repetition IS the count
            # syntax (`[NML2057, NML2057]` = exactly two findings).
            # Containment let an example silently start producing
            # ADDITIONAL errors; set equality let one that demonstrated two
            # same-code findings quietly degrade to one — both are the
            # documentation drift this harness exists to catch. Scoped to
            # the sample's own findings (line-path prefix):
            # `schema=docs/errors/schemas-bad` fences deliberately load a
            # broken schema dir whose OWN errors are context, not the
            # example's claim. Warnings are exempt (they render
            # `warning[...]` and don't fail the check); `expect-output` is
            # the tool for asserting them.
            declared = declared_codes(expected_error)
            if not declared:
                # Prose expectation (`expect-error='tabs are not permitted'`):
                # a message-TEXT claim, asserted by containment — the code
                # contract below only governs code-list annotations.
                if expected_error not in output:
                    return False, (
                        f"error output did not contain {expected_error!r};"
                        f" got:\n{output.strip()}"
                    )
                return True, ""
            produced = sample_codes(output, sample, "error")
            if produced != Counter(declared):
                return False, (
                    f"example's error codes [{render_counts(produced)}] != declared"
                    f" [{render_counts(Counter(declared))}]; got:\n{output.strip()}"
                )
            return True, ""
        expected_output = block.value("expect-output")
        if expected_output is not None:
            if not expected_output:
                # An empty expectation asserts nothing; prose containment
                # would degrade to always-true. A doc-author error, loudly.
                return False, "expect-output= is empty — declare a code list or output text"
            # Both modes require exit 0: `expect-output` documents warnings,
            # and a documented warning that starts ERRORING is the worst
            # drift shape — a substring check alone kept passing on the
            # error text.
            if proc.returncode != 0:
                return False, (
                    "expect-output documents warnings, but the check FAILED;"
                    f" got:\n{output.strip()}"
                )
            declared = declared_codes(expected_output)
            if not declared:
                # Prose expectation: a rendered-text claim, containment.
                if expected_output not in output:
                    return False, (
                        f"output did not contain {expected_output!r};"
                        f" got:\n{output.strip()}"
                    )
                return True, ""
            # Code-list mode is the WARNING-side twin of `expect-error`'s
            # multiset contract: the sample's warning-code multiset must
            # equal the declared list, counts included.
            produced = sample_codes(output, sample, "warning")
            if produced != Counter(declared):
                return False, (
                    f"example's warning codes [{render_counts(produced)}] != declared"
                    f" [{render_counts(Counter(declared))}]; got:\n{output.strip()}"
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
    # The recorded outputs show the sentences' Unicode spelling: a recorded
    # output is the one a reader with a UTF-8 terminal sees.
    env["NML_UNICODE"] = "1"
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
        text = path.read_text(encoding="utf-8")
        if bad := banned_tokens_in(text):
            failures.append((where, f"banned legacy token(s): {', '.join(bad)}"))
            continue
        if bad := reserved_names_in(text):
            failures.append((where, f"reserved name(s): {', '.join(bad)}"))
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
        # A chapter holding a package manifest is MANIFEST-GOVERNED (RFC 0019
        # item 0, D-0d-1): its files validate under the manifest's bindings,
        # and `--schema` beside a governing binding is a usage error. The
        # chapter directory is the universe (`--root`).
        has_manifest = any(f.name.endswith(".package.nml") for f in files)
        for path in files:
            checked += 1
            where = path.relative_to(REPO).as_posix()
            text = path.read_text(encoding="utf-8")
            if bad := banned_tokens_in(text):
                failures.append((where, f"banned legacy token(s): {', '.join(bad)}"))
                continue
            if bad := reserved_names_in(text):
                failures.append((where, f"reserved name(s): {', '.join(bad)}"))
                continue
            if path.name.endswith(".model.nml"):
                cmd = [str(nml_bin()), "validate", str(path)]
            elif has_manifest:
                cmd = [str(nml_bin()), "check", "--root", str(chapter), str(path)]
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

# Execution budget for an already-built program. Deliberately generous: the
# recipes and chapter apps run in well under a second each, so this is not a
# performance bound — it exists to catch a genuine hang. A developer machine
# runs this alongside its own compiles (and an editor's rust-analyzer), where
# a sub-second process can starve for minutes; a tight budget there reports a
# busy machine as a broken recipe, which is the misdiagnosis this file has
# already paid for twice. CI runs alone and never approaches it.
PROGRAM_TIMEOUT = 300


def cargo_binaries(
    args: list[str], timeout: int
) -> tuple[dict[tuple[str, str], str], str | None]:
    """Build with cargo **once** and return `{(kind, name): executable}` from
    cargo's own JSON artifact stream, plus an error string on failure.

    Every runner here used to invoke `cargo run` per program. Cargo holds an
    **exclusive lock on the build directory** for the whole of each
    invocation, so those loops serialized behind any other cargo on the
    machine (a parallel test run, rust-analyzer) — which is how a
    seconds-long recipe check hit a 300s timeout and reported as a hung
    recipe when it had simply never been given the lock. Building once and
    executing the produced binaries directly removes cargo from the hot
    loop: one lock acquisition per runner, no per-program cargo overhead,
    and each program's timeout finally measures the program.

    Paths come from cargo's artifact messages rather than an assumed
    `target/debug/...` layout, so profile, target-dir, and cross-compile
    changes cannot silently break the lookup. `kind` is cargo's own target
    kind (`bin`, `example`) or `test` for test harnesses, so callers ask for
    exactly the artifact class they mean."""
    code, output = run_cmd(args + ["--message-format=json"], timeout=timeout)
    if code != 0:
        return {}, output.strip()
    binaries: dict[tuple[str, str], str] = {}
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        exe, target = msg.get("executable"), msg.get("target") or {}
        if not exe:
            continue
        name = target.get("name", "")
        # A test harness is flagged by profile, not kind (its kind stays
        # `lib`/`test`), so it is classified first.
        if (msg.get("profile") or {}).get("test"):
            binaries[("test", name)] = exe
        else:
            for kind in target.get("kind") or []:
                binaries[(kind, name)] = exe
    return binaries, None


def run_cookbook() -> tuple[int, int, list[tuple[str, str]]]:
    """Run EVERY cookbook example (auto-enumerated — a new recipe can't be
    forgotten) plus the crate's tests (the TOML-equivalence and schema-test
    recipes). Each example must print `recipe OK`; stdin is closed so the
    embed-lsp recipe's server exits on EOF. Returns (checked, passed,
    failures).

    `--no-run` builds the examples *and* the crate's test harnesses in the
    single cargo invocation [`cargo_binaries`] documents, so the whole
    cookbook check costs one build-directory lock."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    examples_dir = REPO / COOKBOOK_DIR / "examples"
    examples = sorted(p.stem for p in examples_dir.glob("*.rs"))
    if not examples:
        return 1, 0, [(COOKBOOK_DIR, "no cookbook examples found — wiring broken?")]

    binaries, error = cargo_binaries(
        ["cargo", "test", "--no-run", "-p", "nml-cookbook"], timeout=900
    )
    if error is not None:
        # One build failure is one finding, not a dozen identical ones.
        return len(examples) + 1, 0, [("cookbook:build", error)]
    test_binaries = [
        (name, exe) for (kind, name), exe in binaries.items() if kind == "test"
    ]

    for name in examples:
        checked += 1
        exe = binaries.get(("example", name))
        if exe is None:
            failures.append(
                (f"cookbook:{name}", "cargo built no executable for this example")
            )
            continue
        code, output = run_cmd([exe], timeout=PROGRAM_TIMEOUT, stdin=subprocess.DEVNULL)
        if code != 0:
            failures.append((f"cookbook:{name}", output.strip()))
        elif "recipe OK" not in output:
            failures.append((f"cookbook:{name}", f"missing 'recipe OK' marker; got:\n{output.strip()}"))
        else:
            passed += 1
    # The crate's own tests (TOML-equivalence, schema-test recipes) ride the
    # same single build above and run as plain binaries too — counted as one
    # check, but any failing binary is named.
    checked += 1
    test_failures = []
    for name, exe in sorted(test_binaries):
        # cwd is part of the contract: `cargo test` runs a harness from its
        # PACKAGE root, and these tests resolve fixtures relative to it.
        # Running them from the repo root instead still passes, but makes
        # them walk the whole tree (`target/` included) — 0.0s becomes
        # ~15s, and under load that crossed the timeout and looked like a
        # hang. `cargo run` does not relocate cwd, so the example binaries
        # above correctly stay at the repo root.
        code, output = run_cmd(
            [exe],
            timeout=PROGRAM_TIMEOUT,
            cwd=REPO / COOKBOOK_DIR,
            stdin=subprocess.DEVNULL,
        )
        if code != 0:
            test_failures.append(f"{name}: {output.strip()}")
    if test_failures:
        failures.append(("cookbook:tests", "\n".join(test_failures)))
    elif not test_binaries:
        failures.append(("cookbook:tests", "cargo built no test binaries — wiring broken?"))
    else:
        passed += 1
    return checked, passed, failures


def run_tutorial_apps() -> tuple[int, int, list[tuple[str, str]]]:
    """Compile AND run each tutorial chapter program, asserting the output its
    page claims. Returns (checked, passed, failures).

    One build for every chapter (see [`cargo_binaries`]), then each program
    runs as a plain binary **from its own chapter directory** — the programs
    read their `.nml` files by relative path, so the cwd is part of what is
    being tested."""
    checked = passed = 0
    failures: list[tuple[str, str]] = []
    binaries, error = cargo_binaries(
        ["cargo", "build"] + [arg for pkg, _, _ in TUTORIAL_APPS for arg in ("-p", pkg)],
        timeout=900,
    )
    if error is not None:
        return len(TUTORIAL_APPS), 0, [("tutorial-apps:build", error)]
    for package, chapter, expect in TUTORIAL_APPS:
        checked += 1
        exe = binaries.get(("bin", package))
        if exe is None:
            failures.append((package, "cargo built no binary for this package"))
            continue
        code, output = run_cmd([exe], timeout=PROGRAM_TIMEOUT, cwd=REPO / chapter)
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

# THE section-header grammar, shared by the census split and the
# order/coverage walk. Two parsers disagreed here once: a header with
# trailing text satisfied a loose coverage match while the strict census
# split folded its fences into the previous section. Near-miss headers are
# a hard failure (see `check_error_index`), never a reclassification.
ERROR_HEADER_RE = re.compile(r"^## (NML\d{4})\s*$", re.M)


def error_index_census(index_text: str) -> tuple[list[tuple[str, str]], str]:
    """Per-section example coverage over the error index.

    Two facts per `## NML####` section: does it carry at least one
    EXECUTED fence, and does any code-declaring fence declare the
    section's own code? The second is a hard failure — a section whose
    examples all demonstrate *other* codes is documentation drift (the
    reader came for this code and the example shows something else).
    Sections with only clean (`nml check`, no expectation) fences count
    as covered — they demonstrate the fixed spelling. Example-free
    sections are a printed census stat, not a failure: some codes are
    legitimately example-free (bounds like NML0007, tombstones)."""
    failures: list[tuple[str, str]] = []
    parts = ERROR_HEADER_RE.split(index_text)
    covered = own_proving = 0
    example_free: list[str] = []
    for code, body in zip(parts[1::2], parts[2::2]):
        # Line-anchored (with fix-1's indent tolerance): an unanchored match
        # would count fence spellings quoted mid-prose as examples. An
        # executed transcript is an example too — the universe codes
        # (NML2080–NML2089) demonstrate a workspace, not a single file.
        fences = re.findall(r"^[ \t]*```nml check[^\n]*", body, flags=re.M)
        fences += re.findall(r"^[ \t]*```text transcript=[^\n]*", body, flags=re.M)
        if not fences:
            example_free.append(code)
            continue
        covered += 1
        declaring = [f for f in fences if re.search(r"NML\d{4}", f)]
        if not declaring:
            continue
        if any(code in f for f in declaring):
            own_proving += 1
        else:
            failures.append(
                (
                    ERROR_INDEX,
                    f"{code}: none of its {len(declaring)} code-declaring"
                    f" fence(s) declares {code} — its examples demonstrate"
                    " other codes",
                )
            )
    total = covered + len(example_free)
    census = (
        f"error-index census: {covered}/{total} sections carry executed"
        f" examples ({own_proving} proving their own code,"
        f" {len(example_free)} example-free)"
    )
    return failures, census


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
    failures = []
    # A near-miss header (`## NML0002 — legacy`, `## NML00021`) is a hard
    # failure: it must not count as coverage here while the census split
    # folds its fences into the previous section.
    for line in index_text.splitlines():
        if re.match(r"## NML\d{4}", line) and not ERROR_HEADER_RE.match(line):
            failures.append(
                (ERROR_INDEX, f"malformed section header {line!r} — must be exactly `## NML####`")
            )
    order = [int(code[len("NML"):]) for code in ERROR_HEADER_RE.findall(index_text)]
    documented = {f"NML{n:04}" for n in order}
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
    census_failures, census = error_index_census(index_text)
    failures.extend(census_failures)
    print(census)
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


# ---------------------------------------------------------------------------
# The canonical-style gate over the repository's own NML.
#
# A language whose specification and tutorials fail its own formatter has no
# canonical style. Every `.nml` file git tracks is held to `nml fmt --check`:
# the specification's examples, every tutorial chapter, every test fixture,
# the editor's fixtures, the fuzz seeds. Two verdicts pass — the file is
# already in canonical style, or the formatter REFUSES it (a formatter does
# not write a guess back over an invalid document, which is how gofmt and
# rustfmt behave too). A third verdict, "would be reformatted", is the
# failure this gate exists to make impossible.
#
# Refusal is only allowed where a document is invalid BY DESIGN. These are
# the trees whose whole purpose is text the language rejects; a refusal
# anywhere else is a file that stopped parsing, and fails.
FMT_REFUSAL_TREES = (
    "tests/fixtures/invalid/",
    "tests/fixtures/dup-names/",
    "tests/fixtures/workspace-brokensrc/",
    "tests/fixtures/workspace-dup/",
    "docs/errors/schemas-bad/",
    "fuzz/seeds/",
)
# No silent shrink: the population is the tracked corpus, and a run that
# checks a handful of files has lost the walk, not passed the gate.
FMT_CORPUS_FLOOR = 150


def check_fmt_corpus() -> tuple[int, int, list[tuple[str, str]]]:
    """Hold every tracked `.nml` file to `nml fmt --check`. Returns
    (checked, clean, failures)."""
    failures: list[tuple[str, str]] = []
    corpus = sorted(f for f in TRACKED if f.endswith(".nml"))
    if not corpus:
        return 0, 0, [("<corpus>", "no tracked .nml files found — the walk is broken")]
    clean = 0
    for rel in corpus:
        path = REPO / rel
        if not path.exists():
            # Tracked, but not in the working tree: the formatter never saw
            # it. Skipping it silently reported a file the gate did not
            # check as one of the corpus's by-design refusals, because the
            # tally is `len(corpus)` (the git list) minus the files that
            # came back clean. The population floor cannot catch it either —
            # it is measured on the same git list.
            failures.append(
                (
                    rel,
                    "tracked but missing from the working tree, so"
                    " `nml fmt --check` never saw it",
                )
            )
            continue
        # One file at a time and BY NAME: the verdict is the formatter's
        # alone, with no directory walk and no universe in the way.
        try:
            proc = subprocess.run(
                [str(nml_bin()), "fmt", "--check", str(path)],
                capture_output=True,
                text=True,
                timeout=60,
                cwd=REPO,
            )
        except subprocess.TimeoutExpired:
            failures.append((rel, "nml fmt did not finish within 60s"))
            continue
        if proc.returncode == 0:
            clean += 1
            continue
        merged = (proc.stdout + proc.stderr).strip()
        refused = "error[NML" in merged
        if refused and rel.startswith(FMT_REFUSAL_TREES):
            continue
        if refused:
            failures.append(
                (rel, "no longer parses, so `nml fmt` refuses it:\n" + merged)
            )
        else:
            failures.append(
                (
                    rel,
                    "not in canonical style — run `nml fmt` and review the diff,"
                    " or say why this file may not be canonical:\n" + merged,
                )
            )
    if len(corpus) < FMT_CORPUS_FLOOR:
        failures.append(
            (
                "<corpus>",
                f"only {len(corpus)} tracked .nml files — the corpus shrank past"
                f" the floor of {FMT_CORPUS_FLOOR}",
            )
        )
    return len(corpus), clean, failures


LIMITS_GUIDE = "docs/guides/validate-in-ci.md"
LIMITS_BEGIN = "<!-- nml limits: begin — GENERATED by scripts/docs_test.py from `nml limits --json`; NML_UPDATE_GOLDEN=1 rewrites it, never edit by hand -->"
LIMITS_END = "<!-- nml limits: end -->"


def render_limits_table(rows: list[dict]) -> str:
    """The published bounds as the guide's table, from `nml limits --json`."""
    lines = [
        "| bound | value | reach | guards | surface | what |",
        "|---|---|---|---|---|---|",
    ]
    for r in rows:
        if r.get("type") != "limit" or not r.get("published"):
            continue
        cells = [f"`{r['name']}`", str(r["value"]), r["reach"], r["guards"], r["surface"], r["what"]]
        assert all("|" not in c for c in cells), f"a pipe in a limits cell: {cells}"
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def check_limits_block() -> list[tuple[str, str]]:
    """The guide's published-bounds table is GENERATED: the block
    between the two markers must equal what `nml limits --json` says now,
    so the prose cannot rot the way a hand-kept table does.
    NML_UPDATE_GOLDEN=1 rewrites it, as for the CLI's goldens."""
    path = REPO / LIMITS_GUIDE
    text = path.read_text(encoding="utf-8")
    begin = text.find(LIMITS_BEGIN)
    end = text.find(LIMITS_END)
    if begin < 0 or end < 0 or end < begin:
        return [(LIMITS_GUIDE, "the generated `nml limits` block markers are missing")]
    code, out = run_cmd([str(nml_bin()), "limits", "--json"])
    if code != 0:
        return [(LIMITS_GUIDE, f"`nml limits --json` failed ({code}): {out[:200]}")]
    rows = [json.loads(line) for line in out.splitlines() if line.strip()]
    expected = render_limits_table(rows)
    head = text[: begin + len(LIMITS_BEGIN)] + "\n"
    actual = text[begin + len(LIMITS_BEGIN) + 1 : end]
    if actual == expected:
        return []
    if "NML_UPDATE_GOLDEN" in os.environ:  # set = update, as the Rust goldens read it
        path.write_text(head + expected + text[end:], encoding="utf-8")
        return []
    return [
        (
            LIMITS_GUIDE,
            "the published-bounds table drifted from `nml limits --json` — "
            "`NML_UPDATE_GOLDEN=1 python3 scripts/docs_test.py` rewrites it",
        )
    ]


WIRE_SHAPE_PATH = "docs/json/nml-ndjson-v1.shape.txt"
WIRE_SHAPE_HEADER = (
    "# The --json wire's SHAPE — GENERATED by scripts/docs_test.py from "
    + NDJSON_SCHEMA_PATH
    + ": every fact a strict consumer's validation turns on (row types, keys,"
    " required sets, types, references, enumerations, constants, patterns), prose"
    " stripped, one line each; the revision is the stamp below, stated once. A change"
    " here at an unchanged revision, or a revision without one, fails the docs gate;"
    " NML_UPDATE_GOLDEN=1 regenerates it after nml-cli/src/out.rs REVISION and the"
    " schema's $defs/revision moved by one — never edit by hand"
)
# Keys that carry prose, not shape.
WIRE_PROSE_KEYS = frozenset({"$schema", "$id", "title", "$comment", "description"})
# Keys whose value is a SET (order carries nothing): an addition may grow one.
WIRE_SET_KEYS = frozenset({"required", "enum", "oneOf", "anyOf", "type"})
WIRE_LEDGER = "CHANGELOG.md"
# One CHANGELOG entry per revision, the latest first — the additions ledger.
WIRE_LEDGER_RE = re.compile(r"^- \*\*`--json` formatVersion (\d+), revision (\d+)\.?\*\*", re.M)


def wire_shape(schema: dict) -> list[str]:
    """The schema's shape as sorted `path = value` lines: every leaf and every
    set-valued key (a scalar `type` is spelled as the one-element set it
    is, so widening it is an addition), prose keys skipped, the two
    constants excluded — the stamp states them once."""
    lines: list[str] = []

    def walk(node: object, path: str) -> None:
        if isinstance(node, dict):
            for key in sorted(node):
                if key in WIRE_PROSE_KEYS:
                    continue
                child = f"{path}.{key}" if path else key
                if key == "type" and isinstance(node[key], str):
                    walk([node[key]], child)
                else:
                    walk(node[key], child)
        elif isinstance(node, list):
            items = sorted(node, key=lambda i: json.dumps(i, sort_keys=True))
            lines.append(f"{path} = {json.dumps(items, sort_keys=True, separators=(',', ':'))}")
        else:
            lines.append(f"{path} = {json.dumps(node)}")

    walk(schema, "")
    stamped = ("$defs.formatVersion.const = ", "$defs.revision.const = ")
    return sorted(line for line in lines if not line.startswith(stamped))


def parse_wire_shape(text: str) -> tuple[tuple[int, int] | None, list[str]]:
    """(the record's stamp, its shape lines); the stamp is None when the
    record carries none."""
    stamp: tuple[int, int] | None = None
    lines: list[str] = []
    for line in text.splitlines():
        if m := re.fullmatch(r"# formatVersion (\d+), revision (\d+)", line):
            stamp = (int(m[1]), int(m[2]))
        elif line.startswith("#") or not line.strip():
            continue
        else:
            lines.append(line)
    return stamp, lines


def render_wire_shape(stamp: tuple[int, int], lines: list[str]) -> str:
    return (
        f"{WIRE_SHAPE_HEADER}\n# formatVersion {stamp[0]}, revision {stamp[1]}\n"
        + "".join(f"{line}\n" for line in lines)
    )


def wire_additions_only(old: list[str], new: list[str]) -> str | None:
    """None when `new` only ADDS to `old` (a new path, a set that grew);
    else the first fact that moved the other way — a rename, a removal,
    a type change or a narrowed vocabulary, which is formatVersion's
    business, never the revision's."""

    def table(lines: list[str]) -> dict[str, str]:
        return {path: value for path, _, value in (line.partition(" = ") for line in lines)}

    was, now = table(old), table(new)
    for path, value in was.items():
        if path not in now:
            return f"`{path}` is gone"
        if now[path] == value:
            continue
        key = path.rsplit(".", 1)[-1]
        if key in WIRE_SET_KEYS and value.startswith("[") and now[path].startswith("["):
            members = lambda v: {json.dumps(i, sort_keys=True) for i in json.loads(v)}  # noqa: E731
            if members(value) <= members(now[path]):
                continue
            return f"`{path}` narrowed from {value} to {now[path]}"
        return f"`{path}` changed from {value} to {now[path]}"
    return None


def wire_check_self_test() -> str | None:
    """The wire check must BITE before it is trusted: the shape spells a
    scalar `type` as the one-element set it is; an identical shape, a new
    path and a grown set are additions; a gone path, a changed scalar and
    a narrowed set are not. None when the helpers behave; else what they
    got wrong."""
    shaped = wire_shape({"$defs": {"a": {"type": "string", "description": "prose"}}})
    if shaped != ['$defs.a.type = ["string"]']:
        return f"a scalar type is not spelled as a set, or prose leaked: {shaped}"
    base = ['a.enum = ["x","y"]', "a.k = 1", 'a.required = ["k"]']
    cases: list[tuple[list[str], str | None]] = [
        (base, None),
        (base + ["a.new = 2"], None),
        (['a.enum = ["x","y","z"]', "a.k = 1", 'a.required = ["k"]'], None),
        (["a.k = 1", 'a.required = ["k"]'], "is gone"),
        (['a.enum = ["x","y"]', "a.k = 2", 'a.required = ["k"]'], "changed"),
        (['a.enum = ["x"]', "a.k = 1", 'a.required = ["k"]'], "narrowed"),
    ]
    for new, expect in cases:
        verdict = wire_additions_only(base, new)
        if expect is None and verdict is not None:
            return f"an addition was refused: {verdict}"
        if expect is not None and (verdict is None or expect not in verdict):
            return f"a non-addition passed or was misnamed: {new} -> {verdict!r}"
    prose = 'row is `{"type":"contract","formatVersion":1,"revision":2,…}` and\nthe next line says "revision":3 —\n'
    if prose_revision_faults(prose, 3) != [(1, '"revision":2')]:
        return f"the prose rule misses a stale revision or flags a current one: {prose_revision_faults(prose, 3)}"
    if prose_revision_faults(prose, 2) != [(2, '"revision":3')]:
        return "the prose rule does not read the writer's revision as the answer"
    return None


def check_wire_revision() -> tuple[list[tuple[str, str]], str]:
    """The `--json` contract's revision, ONE number by construction. The
    writer (`nml-cli/src/out.rs` REVISION, read from `nml version --json`'s
    first row) is the source. Held to it: the schema's `$defs/revision`
    const; the generated shape record — a shape change at an unchanged
    revision, a revision without one, a jump of more than one and a
    change that is not an addition are each refused, under
    NML_UPDATE_GOLDEN=1 too (the regeneration is the reviewed diff, and
    its diff IS the additions the CHANGELOG entry names); and the
    CHANGELOG ledger — exactly one `--json formatVersion F, revision R`
    entry per revision of the writer's format, the latest first; and the
    PROSE, every tracked document that prints the contract row to a
    reader. Returns (failures, the census line)."""
    failures: list[tuple[str, str]] = []
    schema = (SCHEMA_FORMAT_VERSION, SCHEMA_REVISION)
    # 1. the writer
    code, out = run_cmd([str(nml_bin()), "version", "--json"])
    head: dict = {}
    if code == 0 and out.strip():
        try:
            head = json.loads(out.splitlines()[0])
        except json.JSONDecodeError:
            head = {}
    writer = (head.get("formatVersion"), head.get("revision"))
    if head.get("type") != "contract" or writer != schema:
        failures.append(
            (
                NDJSON_SCHEMA_PATH,
                f"the writer's contract row says formatVersion {writer[0]}, revision"
                f" {writer[1]} (`nml version --json`, row one) and the schema's `$defs` say"
                f" {schema[0]}, {schema[1]} — one source, nml-cli/src/out.rs"
                " FORMAT_VERSION/REVISION; the schema follows it",
            )
        )
    # 2. the shape record
    generated = wire_shape(NDJSON_SCHEMA)
    record = REPO / WIRE_SHAPE_PATH
    update = "NML_UPDATE_GOLDEN" in os.environ
    rendered = render_wire_shape(schema, generated)
    regenerate = "`NML_UPDATE_GOLDEN=1 python3 scripts/docs_test.py` regenerates this record"
    if not record.is_file():
        if update:
            record.write_text(rendered, encoding="utf-8")
        else:
            failures.append((WIRE_SHAPE_PATH, f"missing — {regenerate}"))
    else:
        stamp, committed = parse_wire_shape(record.read_text(encoding="utf-8"))
        if stamp is None:
            failures.append((WIRE_SHAPE_PATH, f"no `# formatVersion F, revision R` stamp — {regenerate}"))
        elif committed == generated:
            if stamp != schema:
                failures.append(
                    (
                        WIRE_SHAPE_PATH,
                        f"the revision moved (formatVersion {stamp[0]}, revision {stamp[1]} ->"
                        f" {schema[0]}, {schema[1]}) but the wire's shape did not: a revision"
                        " counts wire additions, a prose change moves nothing — put the"
                        " number back",
                    )
                )
        elif stamp == schema:
            first = next((l for l in generated if l not in committed), None) or next(
                l for l in committed if l not in generated
            )
            failures.append(
                (
                    WIRE_SHAPE_PATH,
                    f"the wire's shape changed at an unchanged revision (formatVersion"
                    f" {stamp[0]}, revision {stamp[1]}; first: {first}) — move REVISION in"
                    " nml-cli/src/out.rs and `$defs/revision` in the schema by one, add the"
                    f" CHANGELOG entry `--json formatVersion {stamp[0]}, revision"
                    f" {stamp[1] + 1}`, then {regenerate}",
                )
            )
        elif (why := wire_additions_only(committed, generated)) is not None:
            failures.append(
                (
                    WIRE_SHAPE_PATH,
                    f"the change is not an addition ({why}): a rename, a removal, a type"
                    " change or a narrowed vocabulary moves formatVersion and the schema's"
                    " file name, never the revision",
                )
            )
        elif stamp[0] != schema[0] or stamp[1] + 1 != schema[1]:
            failures.append(
                (
                    WIRE_SHAPE_PATH,
                    f"the revision moved from formatVersion {stamp[0]}, revision {stamp[1]}"
                    f" to {schema[0]}, {schema[1]}: one round of additions moves it by"
                    " exactly one, and a formatVersion change starts a new schema file",
                )
            )
        elif update:
            record.write_text(rendered, encoding="utf-8")
        else:
            failures.append(
                (
                    WIRE_SHAPE_PATH,
                    f"stale: the schema added to the wire at revision {schema[1]} —"
                    f" {regenerate}; its diff IS the addition list the CHANGELOG entry names",
                )
            )
    # 3. the ledger
    ledger = (REPO / WIRE_LEDGER).read_text(encoding="utf-8")
    ours = [int(r) for f, r in WIRE_LEDGER_RE.findall(ledger) if int(f) == schema[0]]
    want = list(range(schema[1], 0, -1))
    if ours != want:
        failures.append(
            (
                WIRE_LEDGER,
                f"the `--json formatVersion {schema[0]}, revision N` entries read {ours} top"
                f" to bottom; the writer is at revision {schema[1]}, so they must be exactly"
                f" {want} — one entry per revision naming its additions, the latest first",
            )
        )
    # 4. the prose
    #
    # The three holds above cover the writer, the record and the ledger.
    # What a READER is shown was held by nothing: a transcript spells the
    # revision `${REVISION}` and the runner substitutes it, but the two
    # documentation lines that print the contract row as prose are
    # neither substituted nor executed, and both named revision 2 while
    # the writer printed 3.
    for path in doc_files():
        rel = str(path.relative_to(REPO))
        # A design record QUOTES the row as it was — the same reason
        # `BAN_EXEMPT_PATHS` lets it spell removed syntax. (This rule's
        # first run found exactly one such line and nothing else.)
        if any(rel == p or (p.endswith("/") and rel.startswith(p)) for p in BAN_EXEMPT_PATHS):
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for number, shown in prose_revision_faults(text, schema[1]):
            failures.append(
                (
                    f"{rel}:{number}",
                    f"prose prints `{shown}` where the writer is at revision"
                    f" {schema[1]} — this line is what a reader learns the contract"
                    " from, and it is not an executed transcript, so nothing else"
                    " holds it; a revision bump moves it with the ledger entry",
                )
            )
    census = (
        f"wire revision {schema[1]} in sync ({len(generated)} shape lines, {len(ours)} ledger"
        f" entr{'y' if len(ours) == 1 else 'ies'})"
        if not failures
        else "wire revision OUT OF SYNC"
    )
    return failures, census


# ── the wire's ROW-TYPE coverage ──────────────────────────────────────
# The `--json` contract is a PUBLISHED schema, and the only thing that
# ever compared it with the program that writes it is the transcripts
# above. Those transcripts run three verbs (`check`, `binding`,
# `version`), so five of the twelve row types — `parse`, `fmt`, `fix`,
# `explain`, `limit` — were published as a strict consumer contract and
# validated against no real output at all: a field the writer added, a
# vocabulary it widened or a required key it dropped in any of them
# would ship green. The guides are for readers, not for coverage, so the
# missing half runs HERE: one tiny workspace, every verb, every row of
# every stream validated, and the schema's own row list as the bound.
#
# Each entry is (what it is for, argv after `nml`). The fixture is laid
# out by `wire_coverage_fixture`; `--root .` is passed where a universe
# is wanted, because the scratch directory is under this repo's own
# checkout and an underived root would fence at the repository.
WIRE_COVERAGE_RUNS: list[tuple[str, list[str]]] = [
    ("parse", ["parse", "a.nml"]),
    ("validate", ["validate", "a.nml"]),
    ("fmt", ["fmt", "--check", "a.nml"]),
    ("check (bound, clean)", ["check", "--root", ".", "a.nml"]),
    ("check (diagnostic rows)", ["check", "--root", ".", "wrong.nml"]),
    ("check (error row)", ["check", "--root", ".", "absent.nml"]),
    ("fix (fix rows)", ["fix", "--root", ".", "--dry-run", "wrong.nml"]),
    ("binding", ["binding", "--root", ".", "a.nml"]),
    ("explain", ["explain", "NML2007"]),
    ("limits", ["limits"]),
    ("version", ["version"]),
]

WIRE_COVERAGE_MANIFEST = """\
package demo:
    version = "0.1.0"
    formatVersion = 1

[]schema schemas:
    - core:
        file = "core.model.nml"

[]validator validators:
    - core:
        files:
            - "*.nml"
        schemas:
            - core
        strict = true
"""


def wire_coverage_fixture(dir: Path) -> None:
    """The smallest workspace that makes every row type happen: a bound
    file that validates, one that does not (diagnostic rows, and a
    machine-applicable fix for the `fix` rows), and an absent target
    (the `error` row)."""
    (dir / "demo.package.nml").write_text(WIRE_COVERAGE_MANIFEST, encoding="utf-8")
    (dir / "core.model.nml").write_text("model core:\n    v string\n", encoding="utf-8")
    (dir / "a.nml").write_text('core a:\n    v = "x"\n', encoding="utf-8")
    # `vv` is one edit from `v`: an unknown property with a sole
    # candidate, which is what mints a `fix` row with a diff.
    (dir / "wrong.nml").write_text('core b:\n    vv = "x"\n', encoding="utf-8")


def wire_row_types(schema: dict) -> list[str]:
    """Every row type the contract declares, from the top-level `oneOf`."""
    types = []
    for alternative in schema["oneOf"]:
        definition = _resolve_ref(schema, alternative["$ref"])
        types.append(definition["properties"]["type"]["const"])
    return sorted(types)


def check_wire_row_coverage() -> tuple[list[tuple[str, str]], str]:
    """Every row type of the published contract, produced by the real
    binary and validated against the schema. Returns (failures, census).
    """
    failures: list[tuple[str, str]] = []
    seen: dict[str, str] = {}
    rows = 0
    with tempfile.TemporaryDirectory(prefix="nml-wire-") as tmp:
        dir = Path(tmp)
        wire_coverage_fixture(dir)
        for what, args in WIRE_COVERAGE_RUNS:
            code, out = run_cmd([str(nml_bin()), *args, "--json"], cwd=dir)
            if code is None:
                failures.append((f"--json coverage: {what}", out))
                continue
            counted, violation = ndjson_violations(out, NDJSON_SCHEMA)
            rows += counted
            if violation is not None:
                failures.append((f"--json coverage: nml {' '.join(args)} --json", violation))
                continue
            for line in out.splitlines():
                if line.strip():
                    seen.setdefault(json.loads(line)["type"], what)
    declared = wire_row_types(NDJSON_SCHEMA)
    missing = [t for t in declared if t not in seen]
    if missing:
        failures.append(
            (
                NDJSON_SCHEMA_PATH,
                f"row type(s) the contract declares and no run produces: {missing} — the"
                " schema for them is compared with nothing. Add a run to"
                " WIRE_COVERAGE_RUNS that makes each happen, or delete the row type",
            )
        )
    census = (
        f"{len(seen)}/{len(declared)} --json row types produced and validated"
        f" ({rows} rows over {len(WIRE_COVERAGE_RUNS)} verbs)"
    )
    return failures, census


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


# ---------------------------------------------------------------------------
# The wire's numbers where PROSE states them.
#
# The transcripts already spell the revision through `${REVISION}` and refuse
# a literal one. The two pages that teach the consumer rule — "pin
# `formatVersion`, read `revision` from the header, validate strictly only
# against the schema at exactly that revision" — state the header in prose
# instead, and prose is held by nothing: both showed `"revision":2` while the
# writer was at 3, so a consumer following the example pinned a revision that
# fails on the first row of every real run.
#
# Two shapes carry the numbers in prose, and both are checked: a literal
# `contract` row, and the row table's `` `formatVersion` (`1`), `revision`
# (`2` `` cell. A ledger entry ("At **revision 2** the `diagnostic` row
# carries …") is HISTORY and is deliberately not matched by either.
PROSE_CONTRACT_RE = re.compile(
    r'\{[^{}]*"type"\s*:\s*"contract"[^{}]*\}|\{[^{}]*"contract"[^{}]*\}'
)
PROSE_STAMP_CELL_RE = re.compile(
    r"`formatVersion`\s*\(`(\d+)`\),\s*`revision`\s*\(`(\d+)`"
)
PROSE_NUMBER_RE = re.compile(r'"(formatVersion|revision)"\s*:\s*(\d+)')


def prose_wire_numbers(text: str) -> list[tuple[str, int]]:
    """Every (key, value) a page states as the CURRENT wire stamp."""
    found: list[tuple[str, int]] = []
    for row in PROSE_CONTRACT_RE.findall(text):
        found.extend((k, int(v)) for k, v in PROSE_NUMBER_RE.findall(row))
    for fv, rev in PROSE_STAMP_CELL_RE.findall(text):
        found.append(("formatVersion", int(fv)))
        found.append(("revision", int(rev)))
    return found


def check_prose_wire_numbers() -> tuple[list[tuple[str, str]], int]:
    """Every literal wire stamp in the docs' prose is the writer's own.

    Returns the failures and HOW MANY stamps were read: a check that finds
    nothing to check is a check that has been silently disconnected, which
    is the failure the `--json` row counter above exists to catch."""
    want = {
        "formatVersion": SCHEMA_FORMAT_VERSION,
        "revision": int(nml_revision()) if nml_revision().isdigit() else SCHEMA_REVISION,
    }
    failures: list[tuple[str, str]] = []
    read = 0
    for path in doc_files():
        rel = path.relative_to(REPO).as_posix()
        # The design records describe the world as it was at the time they
        # were written, exactly as the banned-token scan exempts them.
        if any(rel == p or (p.endswith("/") and rel.startswith(p)) for p in BAN_EXEMPT_PATHS):
            continue
        text = path.read_text(encoding="utf-8")
        for key, value in prose_wire_numbers(text):
            read += 1
            if value != want[key]:
                failures.append(
                    (
                        str(path.relative_to(REPO)),
                        f"prose states the wire's {key} as {value}; the writer is at"
                        f" {want[key]}. A reader copies this header into their consumer,"
                        " so it has to be the current one",
                    )
                )
    # The check must BITE before it is trusted.
    bitten = prose_wire_numbers('`{"type":"contract","formatVersion":1,"revision":2}`')
    if sorted(bitten) != [("formatVersion", 1), ("revision", 2)]:
        failures.append(("scripts/docs_test.py", f"the prose stamp reader is blind: {bitten}"))
    if prose_wire_numbers("At **revision 2** the `diagnostic` row carries `cause`"):
        failures.append(("scripts/docs_test.py", "the prose stamp reader matched a ledger line"))
    if read == 0:
        failures.append(
            ("scripts/docs_test.py", "no wire stamp found in any doc's prose — the check is not wired")
        )
    return failures, read


# ---------------------------------------------------------------------------
# `cargo install` commands a reader can actually run.
#
# None of this workspace's crates is on crates.io yet — the top-level README
# says so in the same breath as the install command — so a bare
# `cargo install nml-cli` fails with "could not find `nml-cli` in registry
# `crates.io`". Two crate READMEs, the VS Code walkthrough and the
# extension's own "this build bundles no server" message each carried one,
# and the toast is the worst place for it: it is read by somebody whose
# server did not start.
#
# DELETE THIS CHECK on the day the crates are published — and then the
# `--git` forms are the ones that need rewriting.
INSTALL_SURFACES = [
    *DOC_GLOBS,
    "editors/vscode/package.json",
    "editors/vscode/INSTALL.md",
    "editors/vscode/walkthroughs/*.md",
    "editors/vscode/src/*.ts",
]
UNPUBLISHED_CRATES = ("nml-cli", "nml-lsp", "nml-core", "nml-validate", "nml-fmt")
CARGO_INSTALL_RE = re.compile(r"cargo install\b(?P<rest>[^\n`)]*)")


def install_targets(rest: str) -> tuple[set[str], bool]:
    """The crate names an invocation installs, and whether it names a `--git`
    source. Parsed by TOKENS rather than one regex: `--locked --git <url>
    <crate>` defeats a flags-then-crate pattern (the URL is taken for the
    crate), which is how the first cut of this check silently matched
    nothing."""
    tokens = rest.split()
    git = "--git" in tokens
    crates: set[str] = set()
    skip_next = False
    for token in tokens:
        if skip_next:
            skip_next = False
            continue
        if token.startswith("-"):
            skip_next = token in ("--git", "--path", "--version", "--branch", "--tag", "--rev", "--root", "-f")
            continue
        crates.add(token)
    return crates, git


# Adjacent string literals joined by `+` across a line break — the shape a
# message takes in the extension's TypeScript once it is longer than a line.
# The highest-stakes install command in the tree (the "this build bundles no
# server" toast) is written that way, and a scan that reads the file as it
# lies sees two fragments and no command.
TS_CONCAT_RE = re.compile(r'"\s*\+\s*\n?\s*"')


def joined_literals(text: str, suffix: str) -> str:
    """`text` with TypeScript/JavaScript literal concatenation collapsed."""
    return TS_CONCAT_RE.sub("", text) if suffix in (".ts", ".js", ".mjs") else text


def check_install_commands() -> tuple[list[tuple[str, str]], int]:
    """Every `cargo install <this workspace's crate>` names a source that exists."""
    failures: list[tuple[str, str]] = []
    seen = 0
    paths: dict[Path, None] = {}
    for pattern in INSTALL_SURFACES:
        for p in sorted(REPO.glob(pattern)):
            if p.is_file():
                paths[p] = None
    for path in paths:
        rel = path.relative_to(REPO).as_posix()
        if any(rel == p or (p.endswith("/") and rel.startswith(p)) for p in BAN_EXEMPT_PATHS):
            continue
        source = joined_literals(path.read_text(encoding="utf-8"), path.suffix)
        for match in CARGO_INSTALL_RE.finditer(source):
            crates, git = install_targets(match["rest"])
            for crate in sorted(crates & set(UNPUBLISHED_CRATES)):
                seen += 1
                if not git:
                    failures.append(
                        (
                            rel,
                            f"`cargo install … {crate}` with no `--git`: the crate is not"
                            " published, so the command fails with `could not find … in"
                            " registry`. Use the form README.md uses (`cargo install --locked"
                            f" --git https://github.com/nudge-io/nml {crate}`), or publish the"
                            " crate and delete this check",
                        )
                    )
    if seen == 0:
        failures.append(
            ("scripts/docs_test.py", "no `cargo install` of a workspace crate found — the check is not wired")
        )
    if joined_literals('"a " +\n        "b"', ".ts") != '"a b"':
        failures.append(("scripts/docs_test.py", "the literal joiner no longer joins"))
    if joined_literals('"a " +\n        "b"', ".md") == '"a b"':
        failures.append(("scripts/docs_test.py", "the literal joiner runs on prose"))
    for rest, want_crates, want_git in [
        (" --locked --git https://github.com/nudge-io/nml nml-cli", {"nml-cli"}, True),
        (" nml-lsp", {"nml-lsp"}, False),
        (" --locked nml-lsp", {"nml-lsp"}, False),
        (" cargo-deny", {"cargo-deny"}, False),
    ]:
        got_crates, got_git = install_targets(rest)
        if got_crates != want_crates or got_git != want_git:
            failures.append(
                ("scripts/docs_test.py", f"the install parser is wrong on `{rest.strip()}`: {got_crates}, git={got_git}")
            )
    return failures, seen


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
        # The contributor's entry page: every recipe and record it names.
        REPO / "CONTRIBUTING.md",
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

    # Self-test: the gate walks fixed repo globs, so a temp file is never
    # discovered — a seeded string proves the scan itself is alive.
    seeded = reserved_names_in("a CORELATIONx flow and a keystoneVariant")
    if seeded != ["corelation", "keystone"]:
        print(f"docs-test: reserved-name self-test failed: {seeded}", file=sys.stderr)
        return 2
    # Self-test: the `--json` validator refuses what it must and accepts
    # a well-formed row, or nothing it says about the transcripts counts.
    if (why := ndjson_validator_self_test(NDJSON_SCHEMA)) is not None:
        print(f"docs-test: --json validator self-test failed: {why}", file=sys.stderr)
        return 2
    # Self-test: the transcript preparation caps refuse an over-cap ask
    # before creating anything, and an in-cap ask still prepares.
    if (why := prepare_caps_self_test()) is not None:
        print(f"docs-test: preparation-cap self-test failed: {why}", file=sys.stderr)
        return 2
    # Self-test: the wire-revision check tells an addition from a
    # rename, a removal or a narrowing, or nothing it says counts.
    if (why := wire_check_self_test()) is not None:
        print(f"docs-test: wire-revision self-test failed: {why}", file=sys.stderr)
        return 2

    checked = passed = 0
    unverified = 0
    transcripts = transcripts_passed = 0
    rust_synced = 0
    reserved_docs = reserved_files = 0
    failures: list[tuple[str, str]] = []

    for path in doc_files():
        rust_checked, rust_failures = check_rust_source_blocks(path)
        rust_synced += rust_checked
        failures.extend(rust_failures)
        for transcript in extract_transcripts(path):
            transcripts += 1
            ok, detail = run_transcript(transcript)
            if ok:
                transcripts_passed += 1
            else:
                failures.append((transcript.where(), detail))
        # as_posix: ban paths use forward slashes; a Windows checkout must
        # not un-exempt the RFCs (or exempt nothing) via backslash paths.
        rel = path.relative_to(REPO).as_posix()
        exempt = any(
            rel == p or (p.endswith("/") and rel.startswith(p)) for p in BAN_EXEMPT_PATHS
        )
        # The reserved-name scan has NO exemption: the design records are
        # exactly where the names crept in.
        reserved_docs += 1
        if bad := reserved_names_in(path.read_text(encoding="utf-8")):
            failures.append((rel, f"reserved name(s): {', '.join(bad)}"))
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
    reserved_files += files_checked
    failures.extend(file_failures)
    tut_checked, tut_passed, tut_failures = check_tutorial_files()
    reserved_files += tut_checked
    failures.extend(tut_failures)
    apps_checked, apps_passed, app_failures = run_tutorial_apps()
    failures.extend(app_failures)
    cb_checked, cb_passed, cb_failures = run_cookbook()
    failures.extend(cb_failures)
    fmt_checked, fmt_clean, fmt_failures = check_fmt_corpus()
    failures.extend(fmt_failures)
    failures.extend(check_error_index())
    failures.extend(check_guide_links())
    prose_failures, prose_stamps = check_prose_wire_numbers()
    failures.extend(prose_failures)
    install_failures, install_commands = check_install_commands()
    failures.extend(install_failures)
    limits_failures = check_limits_block()
    failures.extend(limits_failures)
    wire_failures, wire_census = check_wire_revision()
    failures.extend(wire_failures)
    row_failures, row_census = check_wire_row_coverage()
    failures.extend(row_failures)

    for where, detail in failures:
        print(f"FAIL {where}")
        for line in detail.splitlines():
            print(f"     {line}")

    # The --json contract check must have RUN: transcripts under --json
    # that validated no row means the wiring is gone, not a clean gate
    # (a mutant that skipped the validation passed the whole gate).
    if transcripts_passed and json_rows_validated == 0:
        print(
            "docs-test: no --json row was validated against the schema (the contract check"
            " is not wired)",
            file=sys.stderr,
        )
        return 2
    print(
        f"docs-test: {passed}/{checked} verified blocks passed,"
        f" {transcripts_passed}/{transcripts} transcripts executed"
        f" ({json_rows_validated} --json rows validated against {NDJSON_SCHEMA_PATH}),"
        f" {files_passed}/{files_checked} example files passed,"
        f" {tut_passed}/{tut_checked} tutorial fixtures passed,"
        f" {apps_passed}/{apps_checked} tutorial programs passed,"
        f" {cb_passed}/{cb_checked} cookbook recipes passed,"
        f" {fmt_clean}/{fmt_checked} tracked .nml files in canonical style"
        f" ({fmt_checked - fmt_clean} refused as invalid by design),"
        f" {rust_synced} rust listings source-synced,"
        f" {1 - len(limits_failures)}/1 generated limits table in sync,"
        f" {prose_stamps} wire stamps in prose checked,"
        f" {install_commands} cargo-install commands checked,"
        f" {wire_census},"
        f" {row_census},"
        f" {unverified} untagged/fragment blocks not verified;"
        f" {reserved_docs} docs + {reserved_files} example files scanned"
        f" for reserved names"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
