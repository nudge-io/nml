#!/usr/bin/env python3
"""RFC 0025 Phase 1 — the two-binary compose oracle.

Compares `nml check --dump-compose` output between an ORACLE binary (built
from the tag cut at the end of Phase 1, sink and dump on both sides) and a
CANDIDATE (the working tree) over:

  * a harvested corpus (NML_CORPUS_DUMP=<dir> cargo test -p nml-core --lib
    layers  — every battery composition's (schema, source, root,
    declaration), one JSON file each; schema+source concatenate into the
    single-file form `nml check` reads), and
  * the committed layer fixtures (tests/fixtures/layers/**.nml).

A per-test ALLOW-LIST names the intended Phase-3 behavior flips (K, K3,
B2, loser-table rows 1-2, M, P6); an allow-listed entry may differ, and
the report says how many did. ANY other difference fails the run — the
oracle is exact, and the allow-list is the changelog of intended
differences (RFC 0025 Phase 5).

Usage:
  scripts/compose_oracle.py --oracle <nml-bin> [--candidate target/debug/nml]
      [--corpus <dir>] [--allow scripts/compose_oracle_allow.txt]

The oracle is a DEV-TIME tool: CI never holds the tagged binary.
"""

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile

REPO = pathlib.Path(__file__).resolve().parent.parent


def dump(binary: str, nml_file: pathlib.Path) -> dict:
    proc = subprocess.run(
        [binary, "check", "--dump-compose", str(nml_file)],
        capture_output=True,
        text=True,
        timeout=120,
    )
    # Exit status is irrelevant here (files with findings exit nonzero);
    # the dump JSON on stdout is the observable. A binary that PANICS
    # (no JSON) is reported as such.
    text = proc.stdout
    start = text.find("{")
    if start < 0:
        return {"__no_dump__": True, "stderr": proc.stderr[-2000:]}
    # The dump is the first pretty-printed JSON object; anything after
    # (rendered findings) is not part of the observable.
    decoder = json.JSONDecoder()
    obj, _ = decoder.raw_decode(text[start:])
    return obj


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", required=True)
    ap.add_argument("--candidate", default=str(REPO / "target/debug/nml"))
    ap.add_argument("--corpus", default=None)
    ap.add_argument(
        "--allow", default=str(REPO / "scripts/compose_oracle_allow.txt")
    )
    args = ap.parse_args()

    allow: set[str] = set()
    allow_path = pathlib.Path(args.allow)
    if allow_path.exists():
        for line in allow_path.read_text().splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                allow.add(line)

    cases: list[tuple[str, pathlib.Path]] = []
    tmp = tempfile.TemporaryDirectory(prefix="nml-oracle-")
    tmpdir = pathlib.Path(tmp.name)
    if args.corpus:
        for f in sorted(pathlib.Path(args.corpus).glob("*.json")):
            entry = json.loads(f.read_text())
            single = tmpdir / (f.stem + ".nml")
            single.write_text(entry["schema"] + "\n" + entry["source"])
            cases.append((entry["test"], single))
    for f in sorted((REPO / "tests/fixtures/layers").rglob("*.nml")):
        cases.append((f"fixture:{f.relative_to(REPO)}", f))

    equal = 0
    allowed = 0
    unexpected: list[str] = []
    for name, path in cases:
        a = dump(args.oracle, path)
        b = dump(args.candidate, path)
        if a == b:
            equal += 1
        elif any(tag in name for tag in allow):
            allowed += 1
        else:
            unexpected.append(name)

    print(
        f"oracle: {equal} equal, {allowed} allow-listed diff(s), "
        f"{len(unexpected)} unexpected over {len(cases)} case(s)"
    )
    for name in unexpected[:25]:
        print(f"  UNEXPECTED: {name}")
    return 1 if unexpected else 0


if __name__ == "__main__":
    sys.exit(main())
