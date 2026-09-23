#!/usr/bin/env python3
"""The landing gate: one contract, run identically by CI and by a developer.

WHY THIS EXISTS
---------------
"CI == local" fails the same way in every project: the two lists are written
twice, and the copies drift. The fixes that work all make one of them the
DERIVATIVE of the other rather than its twin — `rust-lang/rust` has CI invoke
`x.py` and keeps the job matrix as data (`src/ci/github-actions/jobs.yml`);
Bazel makes the build graph the contract so `bazel test //...` is the same
command everywhere; a Nix flake's `checks` ARE the CI job, so `nix flake check`
cannot diverge from what the CI runs; cargo-make / `xtask` projects put the
steps in a program and give CI one line to run.

This repository already has the tool contributors use: `just`. So the rule is:

    THE JUSTFILE IS THE ONLY PLACE A GATE COMMAND IS WRITTEN.
    A WORKFLOW MAY NOT RUN A GATE COMMAND — ONLY `just <recipe>`.

`check` below is the ratchet that enforces it: it reads every workflow, and any
`run:` line that is neither recognised infrastructure (checkout, toolchain
install, cache, artifact upload…) nor a `just` recipe that actually exists is a
failure that names the file, the job and the line. A gate added to CI with no
recipe cannot be run locally, so it is refused; a `gate-*` recipe nothing
references is dead weight, so it is refused too.

And the same rule from the READER's side: every `just <recipe>` a tracked file
tells someone to run must be a recipe. One name per gate is only true if the
documents say that name — a second spelling drifts silently, which is how
`scripts/api_record.py` came to tell contributors to run a recipe named
`api-record`, which never existed. CHANGELOG.md is exempt: it records what
the tree USED to be called, on purpose. (This file spells no live command in
its own prose, so it is judged like any other.)

VACUITY GUARDS (`run`)
----------------------
Every measurement this project has had to throw away came from one of four
things, so the runner asserts against all four and prints the evidence:

  1. A gate run against a different tree than the one being reported. The
     runner prints the ABSOLUTE tree path and a digest of the working tree's
     diff against HEAD, at the top of every log and in the summary.
  2. A cloned `target/` whose cargo fingerprints point at the ORIGINAL tree, so
     nothing recompiles and every test passes without being built. Gates
     marked `compiles` assert that `Compiling nml-…` appears.
  3. A killed wrapper leaving its cargo child alive, which appends to a
     truncated log: two runs of the same test binary in one log inflate the
     count. Gates marked `cargo-test` assert no `Running …` line repeats.
  4. A suite that silently shrank. Gates marked `cargo-test` print the
     passed/failed/ignored totals they parsed, so a drop is visible.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path

try:
    import yaml
except ModuleNotFoundError:  # pragma: no cover - environment, not logic
    print(
        "gate.py needs PyYAML to read .github/workflows (pip install pyyaml)",
        file=sys.stderr,
    )
    raise SystemExit(2)

REPO = Path(__file__).resolve().parent.parent
WORKFLOWS = REPO / ".github" / "workflows"
JUSTFILE = REPO / "justfile"


def workflow_files() -> list[Path]:
    """Every file GitHub will run as a workflow, sorted.

    BOTH extensions: GitHub Actions reads `.yml` AND `.yaml`, so a check that
    globs only one of them is a contract with a hole the size of a rename —
    a `ci.yaml` would run in CI and be invisible here. The one place the set
    is spelled."""
    return sorted(
        {p for pattern in ("*.yml", "*.yaml") for p in WORKFLOWS.glob(pattern)}
    )

# ── the contract ─────────────────────────────────────────────────────────
#
# Tiers, in the order a developer should hit them. Each name is a `just`
# recipe; `just gate` runs the DEFAULT tier list end to end.

TIERS: dict[str, list[str]] = {
    # Seconds. The pre-commit hook's territory.
    "fast": ["gate-contract", "gate-lint"],
    # Minutes. What a pull request must be green on before it is opened.
    "core": ["gate-test", "gate-docs", "gate-ext"],
    # Everything else CI runs on one machine. Tens of minutes.
    #
    # Order is deliberate: `gate-minimal-versions` RESOLVES A DIFFERENT
    # DEPENDENCY SET (that is its point), so everything after it pays a full
    # rebuild. It goes last.
    "full": [
        "gate-supply-chain",
        "gate-wasm",
        "gate-package",
        "gate-perf",
        "gate-ext-e2e",
        "gate-ext-package",
        "gate-fuzz",
        "gate-msrv",
        "gate-minimal-versions",
    ],
    # NOT in DEFAULT_TIERS: it needs a checkout of the consuming workspace
    # (NML_DOWNSTREAM). Named here so it is a gate someone can run, rather
    # than a check three review rounds did by hand and nobody wrote down.
    "downstream": ["gate-downstream"],
    # NOT in DEFAULT_TIERS: it installs two pinned tools and classifies
    # against a BASELINE revision, so its verdict is about a change, not a
    # tree. rust-ci.yml `api` runs it on every push; `just gate api` runs
    # it here.
    "api": ["gate-api"],
}

DEFAULT_TIERS = ["fast", "core", "full"]

# What a gate's log is asserted about (the vacuity guards above).
CARGO_TEST_GATES = {"gate-test", "gate-perf", "gate-msrv"}
COMPILES_GATES = {
    "gate-lint",
    "gate-test",
    "gate-wasm",
    "gate-docs",
    "gate-perf",
    "gate-package",
    "gate-msrv",
    "gate-minimal-versions",
}

# Getting a machine into a state where a gate can run is not itself a gate.
# A short, closed list — anything outside it must be `just <recipe>`.
INFRASTRUCTURE = (
    "rustup ",                   # the toolchain, from rust-toolchain.toml
    "rustup",
    "cargo install just ",       # the gate runner itself (version-pinned)
    "cargo install cargo-fuzz",  # the fuzz runner (version-pinned)
    "cargo install cargo-deny",  # the supply-chain runner (version-pinned)
    "Xvfb ",                     # a virtual display for headless Electron
)

# Actions that set a machine up. An action OUTSIDE this list is treated the
# same as a bare command: `uses:` is the other way a gate can enter CI without
# a recipe, and a check that only reads `run:` would never see it — which is
# exactly how `cargo deny` ran in CI for this repository with no local
# equivalent at all.
INFRASTRUCTURE_ACTIONS = (
    "actions/checkout",
    "actions/cache",
    "actions/upload-artifact",
    "actions/download-artifact",
    "actions/setup-node",
    "pnpm/action-setup",
    "dtolnay/rust-toolchain",
    "Swatinem/rust-cache",
)

# A step whose `run` genuinely is a script — release plumbing, a issue
# filer — declares itself with this marker on its FIRST line and says why.
# Explicit, greppable, and impossible to add by accident; the reason lands in
# `gate.py table` so an exemption cannot quietly become a habit.
NOT_A_GATE = "# gate-contract: not-a-gate"


@dataclass
class WorkflowCommand:
    workflow: str
    job: str
    step: str
    line: str


@dataclass
class Exemption:
    workflow: str
    job: str
    step: str
    reason: str


@dataclass
class DocReference:
    path: str
    line: int
    name: str
    text: str


@dataclass
class UndocumentedRecipe:
    name: str
    line: int


@dataclass
class Findings:
    ungated: list[WorkflowCommand] = field(default_factory=list)
    missing_recipes: list[WorkflowCommand] = field(default_factory=list)
    orphan_recipes: list[str] = field(default_factory=list)
    ci_recipes: set[str] = field(default_factory=set)
    exemptions: list[Exemption] = field(default_factory=list)
    dead_doc_references: list[DocReference] = field(default_factory=list)
    undocumented_recipes: list[UndocumentedRecipe] = field(default_factory=list)


# A recipe name as a document spells it. Two shapes, because documents use
# two: inline in prose (`just gate-docs`) and as a command line of its own
# inside a fenced block (`just gate fast core`). A bare `just` in prose
# ("just the walk", "just an alias") matches neither.
RECIPE_NAME = r"([a-z][a-z0-9]*(?:-[a-z0-9]+)*)"
DOC_INLINE = re.compile(r"`just\s+" + RECIPE_NAME)
DOC_COMMAND = re.compile(r"^just\s+" + RECIPE_NAME)

# The ledger says what the tree used to be called; that is its job.
DOC_EXEMPT = {"CHANGELOG.md"}


def doc_recipe_references(line: str) -> set[str]:
    """Every recipe name one line of a document tells someone to run."""
    return set(DOC_INLINE.findall(line)) | set(DOC_COMMAND.findall(line.strip()))


def tracked_text_files() -> list[Path]:
    """Every tracked (or untracked-but-not-ignored) file, so a document
    added with a gate command in it is judged the same as one already
    there. Outside a git checkout there is nothing to enumerate and the
    check is skipped rather than guessed at."""
    try:
        listed = subprocess.run(
            ["git", "ls-files", "-co", "--exclude-standard"],
            cwd=REPO,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return []
    if listed.returncode != 0:
        return []
    return [REPO / name for name in listed.stdout.split("\n") if name.strip()]


def just_recipes() -> dict[str, list[str]]:
    """Every recipe in the justfile, name -> the recipes it depends on.

    A dependency is a run too: `gate-ext` pulls in `gate-ext-deps`, so the
    orphan check must see the whole reachable set, not just what a workflow
    names directly."""
    recipes: dict[str, list[str]] = {}
    for raw in JUSTFILE.read_text().splitlines():
        if raw.startswith((" ", "\t")) or not raw.strip():
            continue
        m = re.match(r"^([a-z][a-z0-9-]*)(?:\s+[^:]*?)?:(?!=)(.*)$", raw)
        if m:
            recipes[m.group(1)] = [
                d for d in m.group(2).split() if re.fullmatch(r"[a-z][a-z0-9-]*", d)
            ]
    return recipes


def recipe_descriptions(text: str | None = None) -> dict[str, tuple[str, int]]:
    """Every recipe in the justfile, name -> (the description `just --list`
    shows for it, its line).

    `just` takes that description from the comment line IMMEDIATELY above the
    recipe, so a new recipe inserted inside another recipe's comment block
    takes that recipe's description away and keeps a fragment of its last
    sentence — silently, because both files still parse."""
    described: dict[str, tuple[str, int]] = {}
    previous = ""
    for number, raw in enumerate((text if text is not None else JUSTFILE.read_text()).splitlines(), 1):
        if not raw.startswith((" ", "\t")) and raw.strip():
            m = re.match(r"^([a-z][a-z0-9-]*)(?:\s+[^:]*?)?:(?!=)", raw)
            if m:
                described[m.group(1)] = (
                    previous[1:].strip() if previous.startswith("#") else "",
                    number,
                )
        previous = raw
    return described


def workflow_steps() -> list[tuple[str, str, str, str]]:
    """(workflow, job, step label, run block) for every step that runs one."""
    steps: list[tuple[str, str, str, str]] = []
    for path in workflow_files():
        doc = yaml.safe_load(path.read_text())
        for job_name, job in (doc.get("jobs") or {}).items():
            for step in job.get("steps") or []:
                run = step.get("run")
                if run:
                    steps.append(
                        (path.name, job_name, step.get("name") or "(unnamed step)", run)
                    )
    return steps


def workflow_actions() -> list[WorkflowCommand]:
    """Every `uses:` step, so a gate cannot enter CI as a third-party action."""
    used: list[WorkflowCommand] = []
    for path in workflow_files():
        doc = yaml.safe_load(path.read_text())
        for job_name, job in (doc.get("jobs") or {}).items():
            for step in job.get("steps") or []:
                action = step.get("uses")
                if action:
                    used.append(
                        WorkflowCommand(
                            path.name, job_name, step.get("name") or "(unnamed step)", action
                        )
                    )
    return used


def is_infrastructure(line: str) -> bool:
    return any(line.startswith(prefix) for prefix in INFRASTRUCTURE)


# A shell line is a LIST of commands, and the check has to see all of them.
# Recognition used to be `line.startswith(...)`, so everything after the
# recognised head was unread: `just gate-lint; curl … | sh` and
# `cargo install just --version 1.46.0 --locked && curl … | sh` both passed.
# `&&` before `&` so a background `&` (extension-build.yml's Xvfb) is not
# split into an empty segment.
CHAIN = re.compile(r"&&|\|\||;|\||\n")
# Substitutions run a command inside another command's arguments, where no
# split can see them. A step that genuinely needs one declares itself with
# NOT_A_GATE, like every other non-gate.
SUBSTITUTION = re.compile(r"\$\(|`")


def commands_in(line: str) -> list[str]:
    """Every command a `run:` line executes, in order."""
    return [segment.strip() for segment in CHAIN.split(line) if segment.strip()]


def check() -> Findings:
    recipes = just_recipes()
    findings = Findings()
    for workflow, job, label, run in workflow_steps():
        lines = [line.strip() for line in run.splitlines() if line.strip()]
        if lines and lines[0].startswith(NOT_A_GATE):
            findings.exemptions.append(
                Exemption(workflow, job, label, lines[0][len(NOT_A_GATE) :].strip(" :-"))
            )
            continue
        for line in lines:
            if SUBSTITUTION.search(line):
                findings.ungated.append(WorkflowCommand(workflow, job, label, line))
                continue
            for command in commands_in(line):
                if is_infrastructure(command):
                    continue
                # The WHOLE command, not its head: a recipe may take
                # arguments (`just gate fast core`), and they are plain
                # words — anything that is not one has already been split
                # out as a separate command or refused as a substitution.
                m = re.fullmatch(
                    r"just\s+([a-z][a-z0-9-]*)(?:\s+[A-Za-z0-9._/=-]+)*", command
                )
                if not m:
                    findings.ungated.append(
                        WorkflowCommand(workflow, job, label, command)
                    )
                    continue
                name = m.group(1)
                findings.ci_recipes.add(name)
                if name not in recipes:
                    findings.missing_recipes.append(
                        WorkflowCommand(workflow, job, label, command)
                    )
    for action in workflow_actions():
        name = action.line.split("@", 1)[0]
        if not name.startswith(INFRASTRUCTURE_ACTIONS):
            findings.ungated.append(action)
    pending = list(findings.ci_recipes)
    for names in TIERS.values():
        pending.extend(names)
    referenced: set[str] = set()
    while pending:
        name = pending.pop()
        if name in referenced:
            continue
        referenced.add(name)
        pending.extend(recipes.get(name, []))
    for name in recipes:
        if name.startswith("gate-") and name not in referenced:
            findings.orphan_recipes.append(name)
    for name, (description, number) in recipe_descriptions().items():
        if not description:
            findings.undocumented_recipes.append(UndocumentedRecipe(name, number))
    for path in tracked_text_files():
        rel = str(path.relative_to(REPO)) if path.is_relative_to(REPO) else str(path)
        if rel in DOC_EXEMPT or not path.is_file() or path.is_symlink():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue  # a binary or unreadable file tells nobody to run anything
        for number, line in enumerate(text.splitlines(), 1):
            for name in sorted(doc_recipe_references(line)):
                if name not in recipes:
                    findings.dead_doc_references.append(
                        DocReference(rel, number, name, line.strip()[:100])
                    )
    return findings


def print_table(findings: Findings) -> None:
    recipes = just_recipes()
    in_ci = findings.ci_recipes
    in_gate = {name for names in TIERS.values() for name in names}
    names = sorted(set(recipes) | in_ci)
    width = max(len(n) for n in names)
    print(f"{'recipe'.ljust(width)}  CI    just gate")
    print(f"{'-' * width}  ----  ---------")
    for name in names:
        print(
            f"{name.ljust(width)}  "
            f"{'yes ' if name in in_ci else '  - '}  "
            f"{'yes' if name in in_gate else ' - '}"
        )
    if findings.exemptions:
        print()
        print("steps exempted from the contract (declared, with a reason):")
        for e in findings.exemptions:
            print(f"  {e.workflow:22} {e.job:16} {e.step:34} {e.reason}")


def report(findings: Findings) -> int:
    failed = False
    for command in findings.ungated:
        failed = True
        print(
            f"gate contract: {command.workflow} job '{command.job}' step "
            f"'{command.step}' runs a command no `just` recipe owns:\n"
            f"    {command.line}\n"
            "  Move it into the justfile as a `gate-*` recipe and call that "
            "instead, or a developer cannot run what CI runs. If the step is "
            f"genuinely not a gate, start its `run` with `{NOT_A_GATE}: <why>`.",
            file=sys.stderr,
        )
    for command in findings.missing_recipes:
        failed = True
        print(
            f"gate contract: {command.workflow} job '{command.job}' calls "
            f"`{command.line}`, but the justfile has no such recipe.",
            file=sys.stderr,
        )
    for name in findings.orphan_recipes:
        failed = True
        print(
            f"gate contract: recipe `{name}` is in no workflow and in no tier "
            "of scripts/gate.py — a gate nobody runs is not a gate.",
            file=sys.stderr,
        )
    for ref in findings.dead_doc_references:
        failed = True
        print(
            f"gate contract: {ref.path}:{ref.line} tells the reader to run "
            f"`just {ref.name}`, and the justfile has no such recipe:\n"
            f"    {ref.text}\n"
            "  Name the recipe that exists. A document is the other half of "
            "the one-name rule: a second spelling drifts silently.",
            file=sys.stderr,
        )
    for recipe in findings.undocumented_recipes:
        failed = True
        print(
            f"gate contract: justfile:{recipe.line} recipe `{recipe.name}` has no "
            "description, so `just --list` — the index every contributor reads — "
            "shows it as a bare name.\n"
            "  Put a one-line `# …` comment directly above it. If the recipe was "
            "added inside ANOTHER recipe's comment block, move it out first: `just` "
            "reads the line immediately above a recipe as its description, so the "
            "insertion takes that recipe's description away.",
            file=sys.stderr,
        )
    return 1 if failed else 0


# ── running the gates ────────────────────────────────────────────────────


def untracked_paths(status_z: bytes) -> list[str]:
    """The `??` entries of `git status --porcelain -uall -z`, in order.

    `-z` because the plain format QUOTES a path with a space or a non-ASCII
    byte in it; `-uall` because the default collapses a whole untracked
    directory to one `dir/` entry, which names no file to read."""
    return sorted(
        entry[3:].decode("utf-8", "surrogateescape")
        for entry in status_z.split(b"\0")
        if entry.startswith(b"?? ")
    )


def fingerprint_digest(diff: bytes, status_z: bytes, read: Callable[[str], bytes]) -> str:
    """The digest of a working tree, given its diff, its `-z` status, and a
    way to read an untracked file.

    UNTRACKED FILE CONTENT IS IN IT, and that is the whole point. A digest of
    the diff and the status alone answers the SAME for two trees that differ
    only inside a file git has never seen — and in this repository that is not
    an exotic case: the whole working state is one large uncommitted change,
    every new source file arrives untracked first, and a review lane that adds
    a file and then edits it reported one digest for both trees (MEASURED,
    twice in a row). A fingerprint that cannot tell two trees apart
    is worse than none: it is a certificate that the wrong tree was measured.

    Pure, so [`fingerprint_self_test`] can drive it without a git repository."""
    hashed = hashlib.sha256(diff + b"\0" + status_z)
    for path in untracked_paths(status_z):
        hashed.update(b"\0" + path.encode("utf-8", "surrogateescape") + b"\0")
        try:
            hashed.update(read(path))
        except OSError as err:
            # A path that cannot be read is still a difference between two
            # trees; record WHICH failure, never silently nothing.
            hashed.update(f"<unreadable: {err.__class__.__name__}>".encode())
    return hashed.hexdigest()[:16]


def tree_fingerprint() -> tuple[str, str]:
    """The tree being gated: its path, and a digest of everything it carries
    that HEAD does not — its diff, its status, and its untracked files' bytes.

    Printed at the top of every gate. A result reported for one tree and
    measured in another is the failure mode this line exists to make
    impossible to miss."""
    try:
        diff = subprocess.run(
            ["git", "diff", "HEAD"],
            cwd=REPO,
            capture_output=True,
            check=False,
        ).stdout
        status_z = subprocess.run(
            ["git", "status", "--porcelain", "-uall", "-z"],
            cwd=REPO,
            capture_output=True,
            check=False,
        ).stdout
    except OSError:
        return str(REPO), "no-git"

    def read(path: str) -> bytes:
        return (REPO / path).read_bytes()

    return str(REPO), f"sha256:{fingerprint_digest(diff, status_z, read)}"


def wipe_fingerprints() -> int:
    """Drop this workspace's cargo fingerprints.

    A `cp -cR` clone inherits fingerprints that name the ORIGINAL tree's
    absolute paths, so cargo believes the crates are fresh and every test
    binary is the one the other tree built: green, and about nothing. Removing
    the workspace crates' fingerprints forces the rebuild that the
    `Compiling nml-…` assertion then checks for."""
    removed = 0
    for target in (REPO / "target", REPO / "fuzz" / "target"):
        for path in target.glob("**/.fingerprint/nml-*"):
            if path.is_dir():
                shutil.rmtree(path, ignore_errors=True)
                removed += 1
    return removed


TEST_RESULT = re.compile(
    r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored",
    re.MULTILINE,
)


# The shapes a gate's OWN failure opens with, in the order a log carries
# them. `error: Recipe … failed` is just's wrapper around the exit code and
# names no cause, so it is skipped.
FAILURE_LEAD = re.compile(r"^(error(\[|:| )|FAIL |Diff in |failures:|\s*---- )")


def first_failure_line(text: str) -> str | None:
    """The first line of a red log that says what went wrong.

    A contributor's first red gate used to print one line — a VACUITY
    warning about build artifacts from another tree — and a path to a log.
    When the recipe fails BEFORE rustc runs (an unformatted file, a missing
    tool), that warning is the only line they get and it is the wrong
    diagnosis. This is the right one, printed beside the log path."""
    for line in text.splitlines():
        if line.startswith("error: Recipe "):
            continue
        if FAILURE_LEAD.match(line):
            return line.strip()
    return None


def audit_log(name: str, text: str, *, failed: bool) -> list[str]:
    """The vacuity assertions over one gate's output.

    `failed` is the recipe's own verdict, and is keyword-ONLY and
    required: a caller that does not state it would silently get the
    green-gate semantics, which is the bug this parameter fixes.

    The recompilation guard asks
    "did this GREEN gate measure anything?" — on a gate that already
    exited non-zero the answer is beside the point, and printing it
    accuses the contributor's build artifacts of a fault their own change
    caused. The duplicate-binary guard stays either way: it is about
    totals, which a red run still reports."""
    problems: list[str] = []
    # `cargo check`/`clippy` say "Checking", `cargo build`/`test` say
    # "Compiling" — both mean rustc really looked at this workspace's crates,
    # which is what the guard is about.
    if not failed and name in COMPILES_GATES and not re.search(r"(Compiling|Checking) nml-", text):
        problems.append(
            "nothing in this workspace recompiled (`Compiling nml-…` never "
            "appeared): the gate may have run against another tree's build "
            "artifacts. Fingerprints are wiped before the first gate; if this "
            "fires, the wipe did not reach this target directory."
        )
    if name in CARGO_TEST_GATES:
        # The BINARY, which is the parenthesised path — not the first word,
        # which is the literal "unittests" for every lib target and would
        # make this guard fire on any workspace with two library crates.
        running = re.findall(r"^\s*Running .*\((\S+)\)", text, re.MULTILINE)
        duplicates = {b for b in running if running.count(b) > 1}
        if duplicates:
            problems.append(
                "the same test binary ran twice in one log "
                f"({sorted(duplicates)}): a survivor process appended to it, "
                "so every total here is inflated."
            )
    return problems


def summarize_tests(text: str) -> str | None:
    rows = [m for m in TEST_RESULT.finditer(text)]
    if not rows:
        return None
    passed = sum(int(m.group(1)) for m in rows)
    failed = sum(int(m.group(2)) for m in rows)
    ignored = sum(int(m.group(3)) for m in rows)
    return f"{passed} passed / {failed} failed / {ignored} ignored"


def run_gates(names: list[str], log_dir: Path) -> int:
    log_dir.mkdir(parents=True, exist_ok=True)
    tree, digest = tree_fingerprint()
    print(f"gate: tree      {tree}")
    print(f"gate: diff      {digest}")
    print(f"gate: recipes   {' '.join(names)}")
    print(f"gate: logs      {log_dir}")
    removed = wipe_fingerprints()
    print(f"gate: wiped {removed} nml-* cargo fingerprint directories")
    print()

    failures: list[str] = []
    for name in names:
        log_path = log_dir / f"{name}.log"
        started = time.monotonic()
        header = (
            f"=== gate {name}\n=== tree {tree}\n=== diff {digest}\n"
            f"=== command just {name}\n"
        )
        with log_path.open("w") as log:
            log.write(header)
            log.flush()
            completed = subprocess.run(
                ["just", name],
                cwd=REPO,
                stdout=log,
                stderr=subprocess.STDOUT,
                check=False,
            )
        elapsed = time.monotonic() - started
        text = log_path.read_text()
        problems = audit_log(name, text, failed=completed.returncode != 0)
        totals = summarize_tests(text)
        status = "GREEN" if completed.returncode == 0 and not problems else "RED"
        detail = f" [{totals}]" if totals else ""
        print(f"{status:5}  {name:22} {elapsed:7.1f}s  exit {completed.returncode}{detail}")
        for problem in problems:
            print(f"       VACUITY: {problem}")
        if status == "RED":
            failures.append(name)
            if completed.returncode != 0 and (why := first_failure_line(text)):
                print(f"       why: {why}")
            print(f"       log: {log_path}")

    print()
    if failures:
        print(f"gate: RED — {', '.join(failures)}")
        return 1
    print(f"gate: GREEN — {len(names)} gates, tree {tree}, diff {digest}")
    return 0


# ── self-test: the ratchet must be able to go RED ────────────────────────

SELF_TEST_WORKFLOWS = {
    # Drift class 1: a gate command typed straight into a workflow.
    "a bare gate command": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test --workspace --locked
""",
    # Drift class 2: a gate arriving as a third-party action, which a check
    # that reads only `run:` would never see — how `cargo deny` came to run
    # in CI here with no local equivalent.
    "a gate as a third-party action": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - uses: SomeOrg/some-check-action@0000000000000000000000000000000000000000
""",
    # Drift class 3: `just` calling a recipe that does not exist.
    "a call to a recipe that does not exist": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - run: just gate-does-not-exist
""",
    # Drift class 4: a second command chained onto a recognised one. A
    # `startswith` check reads only the head, so everything after `;` or
    # `&&` entered CI unexamined.
    "a command chained onto a `just` recipe": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - run: just gate-lint; curl -fsSL https://example.invalid/x.sh | sh
""",
    "a command chained onto recognised infrastructure": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - run: cargo install just --version 1.46.0 --locked && sh ./x.sh
""",
    # Drift class 5: a command substitution, which no split can see into.
    "a command substitution inside a recognised command": """
name: planted
on: {push: {}}
jobs:
  planted:
    runs-on: ubuntu-latest
    steps:
      - run: just $(curl -fsSL https://example.invalid/name)
""",
}

# The same drift, in a file named `.yaml`. GitHub runs both extensions; a
# check that globs one of them is bypassed by a rename, so the self-test
# plants this one under the OTHER name.
SELF_TEST_YAML_SUFFIX = ".yaml"


# Drift class 4: a DOCUMENT that names a recipe nobody has. The planted
# file is a real tracked-tree file, so the walk that finds it is the one
# under test, not a stand-in.
SELF_TEST_DOC_RECIPE = "gate-does-not-exist"
SELF_TEST_DOC_NAME = "zz-gate-self-test.md"
# Composed, not spelled: a literal here would be a live reference in this
# file to a recipe that must not exist, and this file is under the rule.
SELF_TEST_DOC_BODY = (
    f"Run `just {SELF_TEST_DOC_RECIPE}` before opening a pull request.\n"
)

# The line rule itself, both ways: what a document DOES tell someone to run,
# and the prose shapes it must not read as a command.
SELF_TEST_DOC_LINES: tuple[tuple[str, set[str]], ...] = (
    ("run `just gate-docs` on every change", {"gate-docs"}),
    ("just gate fast core", {"gate"}),
    ("    just gate-ext-e2e   # the real editor", {"gate-ext-e2e"}),
    ("`just gate-<name>` is the spelling a workflow uses", {"gate"}),
    ("this is just the walk, not an alias", set()),
    ("not just an arrow but a cycle", set()),
    ("covers list elements too, not just field-level blocks", set()),
)


def doc_rule_self_test() -> int:
    """The line rule, before the walk that uses it: a ratchet whose matcher
    is wrong is a ratchet about nothing."""
    for line, want in SELF_TEST_DOC_LINES:
        got = doc_recipe_references(line)
        if got != want:
            print(
                f"self-test: FAILED — the document rule read {sorted(got)} in "
                f"{line!r}, expected {sorted(want)}.",
                file=sys.stderr,
            )
            return 1
    print(f"self-test: the document rule reads {len(SELF_TEST_DOC_LINES)} lines correctly")
    return 0


# The description rule itself: `just --list` is the contributor's index, and
# a recipe loses its line there silently. The third case is the whole point —
# `stolen` is documented until `thief` is inserted under its comment block.
SELF_TEST_JUSTFILE = """\
# Build it.
build:
    cargo build

undocumented:
    cargo doc

# Run some of them.
# The last line is the description.
test-some ARG:
    cargo test {{ARG}}

# A block that explains at length,
# and ends on the line `just` will show.
thief:
    echo one

stolen:
    echo two
"""

SELF_TEST_DESCRIPTIONS: tuple[tuple[str, str], ...] = (
    ("build", "Build it."),
    ("undocumented", ""),
    ("test-some", "The last line is the description."),
    ("thief", "and ends on the line `just` will show."),
    ("stolen", ""),
)


def recipe_description_self_test() -> int:
    """The description rule, on a justfile that carries every shape: a
    documented recipe, one with none, one with arguments, and the theft —
    a recipe inserted under another's comment block, which leaves the one
    below it with no description at all."""
    got = recipe_descriptions(SELF_TEST_JUSTFILE)
    for name, want in SELF_TEST_DESCRIPTIONS:
        if name not in got or got[name][0] != want:
            print(
                f"self-test: FAILED — the description rule read "
                f"{got.get(name, ('<missing>',))[0]!r} for `{name}`, expected {want!r}.",
                file=sys.stderr,
            )
            return 1
    if len(got) != len(SELF_TEST_DESCRIPTIONS):
        print(
            f"self-test: FAILED — the description rule found {sorted(got)}, "
            f"expected {sorted(n for n, _ in SELF_TEST_DESCRIPTIONS)}.",
            file=sys.stderr,
        )
        return 1
    print(
        f"self-test: the description rule reads {len(SELF_TEST_DESCRIPTIONS)} recipes "
        "correctly, the stolen one included"
    )
    return 0


def fingerprint_self_test() -> int:
    """The tree fingerprint must move for every way one tree differs from
    another — including the one it used to miss.

    Four trees, each differing from the baseline in exactly one thing. All
    five digests must be distinct; the third row is the defect this exists
    for (an untracked file whose CONTENT changed), and it went undetected
    until a review lane hit it by accident."""
    files = {"src/new.ts": b"one"}
    baseline = (b"diff-a", b"M  a.rs\0?? src/new.ts\0", files)
    cases = [
        ("the tracked diff changed", (b"diff-b", baseline[1], files)),
        ("the status changed", (baseline[0], b"M  b.rs\0?? src/new.ts\0", files)),
        ("an UNTRACKED file's content changed", (baseline[0], baseline[1], {"src/new.ts": b"two"})),
        (
            "a second untracked file appeared",
            (baseline[0], b"M  a.rs\0?? src/new.ts\0?? src/other.ts\0", {**files, "src/other.ts": b""}),
        ),
    ]
    seen = {fingerprint_digest(baseline[0], baseline[1], lambda p: baseline[2][p]): "the baseline"}
    for description, (diff, status_z, content) in cases:
        digest = fingerprint_digest(diff, status_z, lambda p: content[p])
        if digest in seen:
            print(
                f"self-test: FAILED — the tree fingerprint cannot tell the baseline from "
                f"a tree where {description}: both are {digest} (same as {seen[digest]}).",
                file=sys.stderr,
            )
            return 1
        seen[digest] = description
    print(f"self-test: the tree fingerprint separates {len(seen)} trees, content and all")
    return 0


def red_report_self_test() -> int:
    """A red gate names its own cause, and does not accuse the build cache.

    Both halves have been wrong here: `cargo fmt --check` failing printed a
    VACUITY line about another tree's artifacts and nothing about the file
    that needed formatting."""
    fmt_log = (
        "=== gate gate-lint\ncargo fmt --all -- --check\n"
        "Diff in /repo/nml-cli/src/scratch.rs:45:\n"
        "error: Recipe `gate-lint` failed on line 119 with exit code 1\n"
    )
    cases: list[tuple[str, str, str | None]] = [
        ("an unformatted file", fmt_log, "Diff in /repo/nml-cli/src/scratch.rs:45:"),
        ("a clippy denial", "Checking nml-core\nerror: unused variable `x`\n", "error: unused variable `x`"),
        ("a docs failure", "FAIL docs/guides/x.md\n     line 3: broken\n", "FAIL docs/guides/x.md"),
        ("a test failure", "Running x\n---- t::a stdout ----\nfailures:\n", "---- t::a stdout ----"),
        ("only just's wrapper", "error: Recipe `g` failed on line 1 with exit code 1\n", None),
    ]
    for description, text, want in cases:
        got = first_failure_line(text)
        if got != want:
            print(f"gate: self-test FAILED — {description}: got {got!r}, want {want!r}")
            return 1
    # The recompilation guard is for a GREEN gate; a red one is not vacuous.
    name = next(iter(COMPILES_GATES))
    if audit_log(name, fmt_log, failed=True):
        print("gate: self-test FAILED — a red gate still reports a recompilation vacuity")
        return 1
    if not audit_log(name, fmt_log, failed=False):
        print("gate: self-test FAILED — the recompilation guard no longer fires on a green gate")
        return 1
    return 0


def self_test() -> int:
    """Prove the contract check is not vacuous.

    A ratchet that has never been seen RED is a comment. This plants a
    workflow that runs a gate command directly — exactly the drift the check
    exists to stop — and fails if the check stays green."""
    if (code := fingerprint_self_test()) != 0:
        return code
    if (code := doc_rule_self_test()) != 0:
        return code
    if (code := recipe_description_self_test()) != 0:
        return code
    if (code := red_report_self_test()) != 0:
        return code
    cases = [
        (f"{description} (.yml)", content, "zz-gate-self-test.yml")
        for description, content in SELF_TEST_WORKFLOWS.items()
    ]
    # Every class again under `.yaml`, the extension a glob forgets.
    cases += [
        (f"{description} (.yaml)", content, "zz-gate-self-test" + SELF_TEST_YAML_SUFFIX)
        for description, content in SELF_TEST_WORKFLOWS.items()
    ]
    for description, content, name in cases:
        planted = WORKFLOWS / name
        if planted.exists():
            print("self-test: refusing to overwrite an existing file", file=sys.stderr)
            return 2
        planted.write_text(content)
        try:
            findings = check()
            caught = [
                c
                for c in findings.ungated + findings.missing_recipes
                if c.workflow == planted.name
            ]
            if not caught:
                print(
                    f"self-test: FAILED — {description} was NOT reported. "
                    "The contract check is vacuous for that drift class.",
                    file=sys.stderr,
                )
                return 1
            print(f"self-test: caught {description} ({caught[0].line})")
        finally:
            planted.unlink()
    name, body, recipe = SELF_TEST_DOC_NAME, SELF_TEST_DOC_BODY, SELF_TEST_DOC_RECIPE
    planted_doc = REPO / name
    if planted_doc.exists():
        print("self-test: refusing to overwrite an existing file", file=sys.stderr)
        return 2
    planted_doc.write_text(body)
    try:
        caught = [
            r for r in check().dead_doc_references if r.path == name and r.name == recipe
        ]
        if not caught:
            print(
                "self-test: FAILED — a document naming a recipe that does not "
                "exist was NOT reported. The contract check is vacuous for "
                "that drift class.",
                file=sys.stderr,
            )
            return 1
        print(f"self-test: caught a document naming a recipe nobody has (just {recipe})")
    finally:
        planted_doc.unlink()
    findings = check()
    if report(findings) != 0:
        print("self-test: FAILED — the real tree does not pass its own check", file=sys.stderr)
        return 1
    print("self-test: and the real tree is green")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check", help="the CI/local divergence ratchet")
    sub.add_parser("table", help="print the CI vs `just gate` table")
    sub.add_parser("self-test", help="prove the ratchet can go RED")
    run = sub.add_parser("run", help="run gates with the vacuity guards")
    run.add_argument("tiers", nargs="*", default=DEFAULT_TIERS)
    run.add_argument("--log-dir", default=str(REPO / "target" / "gate-logs"))
    args = parser.parse_args()

    if args.command == "check":
        findings = check()
        code = report(findings)
        if code == 0:
            print("gate contract: every CI gate is a `just` recipe.")
        return code
    if args.command == "table":
        print_table(check())
        return 0
    if args.command == "self-test":
        return self_test()

    names: list[str] = []
    for tier in args.tiers or DEFAULT_TIERS:
        if tier in TIERS:
            names.extend(n for n in TIERS[tier] if n not in names)
        elif tier not in names:
            names.append(tier)
    return run_gates(names, Path(args.log_dir))


if __name__ == "__main__":
    raise SystemExit(main())
