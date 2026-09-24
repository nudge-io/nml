#!/usr/bin/env python3
"""Reachability census of the public-API record — the review tool behind an
API change.

For every NAMED item in `docs/api/*.api.txt` (the record `api_record.py`
keeps), who outside the item's own crate refers to it: another workspace
crate, a binary target, the CLI, the cookbook and tutorial examples, the
integration tests, fuzz, the docs — and any extra consumer tree given with
`--consumer` (a checkout of the platform that embeds these crates). An item
nothing reaches is a CANDIDATE for `pub(crate)`, never a verdict; one only
tests reach belongs behind a `test-support` feature, never in the record.
The record gate makes every addition VISIBLE (revision +1, a CHANGELOG
line); this census asks the question the gate cannot — does anyone need it?
— so run it before you bump a revision for an addition, with `--item` for
the one line the gate just named.

A census by identifier, not a proof. A member (a method, a field, a variant)
counts as reached only where its owning type is named in the same file, a
free item wherever a file that names the crate names it — in CODE: an
identifier inside a comment or a string is a mention, reported as
`prose-only`, never counted as a reach (`--loose` collapses the two halves,
the wider net a stray sentence can fool). A miss says only that no tree the
census read spells the name; the census reads the trees it is GIVEN, and an
embedder it was not pointed at is not a miss it can see — one narrowing made
from a census without the embedder's checkout broke that embedder's build.
The compiler is the proof: `cargo check` here, and
`NML_DOWNSTREAM=<embedder checkout> just gate downstream` against every
consumer, before a `pub(crate)` lands. This census is what found that the
language server's record was public almost entirely for its own tests.

    python3 scripts/api_reach.py [--consumer DIR ...] [--crate NAME ...] [--item TEXT] [--all] [--loose]
    python3 scripts/api_reach.py --self-test
"""

from __future__ import annotations

import argparse
import collections
import os
import re
import stat
import sys
import tempfile
from pathlib import Path

from api_record import CRATES, ROOT, parse, record_path

SKIP_DIRS = {"target", "node_modules", ".vscode-test", "out", "dist", ".claude", ".git"}
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
# A record line may open with attributes (`#[non_exhaustive] pub enum …`); the
# item behind them is a real one.
ATTR_PREFIX = re.compile(r"^(?:#\[[^\]]*\] )+")
# Names every derive and std trait impl puts in the record: never a reach question.
TRAIT_NOISE = {
    "clone", "fmt", "eq", "ne", "hash", "default", "from", "into", "try_from", "try_into",
    "borrow", "borrow_mut", "to_owned", "clone_into", "to_string", "into_iter", "deref",
    "deref_mut", "type_id", "partial_cmp", "cmp", "as_ref", "as_mut", "drop", "call",
    "poll_ready", "next", "serialize", "deserialize", "from_iter", "extend", "index",
    "index_mut", "from_str", "sum", "product", "Target", "Error", "Future", "Response",
    "Output", "Item",
}
TEST_ROLES = {"tests", "fuzz", "examples"}

# `--consumer DIR` points this census at a tree it does not own, so it reads
# that tree the way the crate's own filesystem leaf reads a schema source
# (`nml_validate::fs`): only REGULAR files, only inside the tree it was
# pointed at, and only under a byte bound — past it the file is REFUSED and
# named, never silently truncated. A FIFO named `*.rs` parks an unguarded
# reader in `read(2)` for as long as nobody writes to it.
MAX_FILE_BYTES = 4 * 1024 * 1024


def owner_of(path: Path) -> tuple[str, str]:
    """(who this file belongs to, its role) — a crate's own `src/` is not a
    consumer of that crate; its bin targets, tests, the CLI, the examples,
    fuzz and the docs are."""
    rel = path.relative_to(ROOT).parts
    if rel[0] == "crates":
        crate = rel[1].replace("-", "_")
        if rel[2] == "src" and (rel[3] == "main.rs" or rel[3] == "bin"):
            return crate, "bin"
        return (crate, "src") if rel[2] == "src" else (crate, "tests")
    if rel[0] == "nml-cli":
        return "nml_cli", "examples" if rel[1] == "examples" else "src"
    if rel[0] == "tests":
        return "tests", "tests"
    if rel[0] == "fuzz":
        return "fuzz", "fuzz"
    if rel[0] == "docs":
        return "docs", "examples" if path.suffix == ".rs" else "docs"
    return "other", "other"


RAW_STRING = re.compile(r'b?r(?P<hashes>#*)"')
CHAR_LITERAL = re.compile(r"b?'(?:\\.[^'\n]*|[^\\'\n])'")


def rust_split(text: str) -> tuple[str, str]:
    """(code, prose) for Rust source: comments (line, block, nested) and
    string, raw-string and char literals are prose; everything else is code.
    A lifetime (`'a`, `'static`) is code — it has no closing quote."""
    code: list[str] = []
    prose: list[str] = []
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        if ch == "/" and text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            prose.append(text[i:end])
            i = end
            continue
        if ch == "/" and text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                elif text.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            prose.append(text[i:j])
            i = j
            continue
        if ch in "rb" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = RAW_STRING.match(text, i)
            if m:
                close = '"' + m.group("hashes")
                end = text.find(close, m.end())
                end = n if end < 0 else end + len(close)
                prose.append(text[i:end])
                i = end
                continue
        if ch == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    break
                j += 1
            prose.append(text[i:j])
            i = j
            continue
        if ch == "'" or (ch == "b" and text.startswith("b'", i)):
            m = CHAR_LITERAL.match(text, i)
            if m:
                prose.append(m.group(0))
                i = m.end()
                continue
        code.append(ch)
        i += 1
    return "".join(code), "".join(prose)


FENCE = re.compile(r"^\s*(?:```|~~~)", re.M)


def markdown_split(text: str) -> tuple[str, str]:
    """(code, prose) for a doc page: a fenced block is code, the sentences
    around it are prose — a name a guide merely discusses is a mention."""
    code: list[str] = []
    prose: list[str] = []
    inside = False
    for line in text.splitlines(keepends=True):
        if FENCE.match(line):
            inside = not inside
            prose.append(line)
            continue
        (code if inside else prose).append(line)
    return "".join(code), "".join(prose)


def readable(path: Path, root: Path | None) -> tuple[str | None, str | None]:
    """(text, why it was refused). Only a REGULAR file, only inside `root`,
    only under [`MAX_FILE_BYTES`]. `stat` never blocks; `open` on a FIFO
    does, so the kind is settled before the file is opened."""
    try:
        if root is not None:
            here = os.path.realpath(path)
            base = os.path.realpath(root)
            if os.path.commonpath([here, base]) != base:
                return None, f"{path}: a link out of {root}"
        st = path.stat()
    except (OSError, ValueError):
        return None, f"{path}: unreadable"
    if not stat.S_ISREG(st.st_mode):
        return None, f"{path}: not a regular file"
    if st.st_size > MAX_FILE_BYTES:
        return None, f"{path}: {st.st_size} bytes, past the {MAX_FILE_BYTES}-byte bound"
    try:
        return path.read_text(encoding="utf-8", errors="replace"), None
    except OSError:
        return None, f"{path}: unreadable"


Tokens = tuple[Path, set[str], set[str]]  # path, code identifiers, prose identifiers


def tokens_of(paths: list[Path], root: Path | None, refused: list[str]) -> list[Tokens]:
    out: list[Tokens] = []
    for path in paths:
        if any(part in SKIP_DIRS for part in path.parts):
            continue
        text, why = readable(path, root)
        if text is None:
            if why is not None:
                refused.append(why)
            continue
        split = markdown_split if path.suffix == ".md" else rust_split
        code, prose = split(text)
        out.append((path, set(IDENT.findall(code)), set(IDENT.findall(prose))))
    return out


File = tuple[Path, str, str, set[str], set[str]]  # path, owner, role, code, prose


def consumers(extra: list[Path], refused: list[str]) -> list[File]:
    files: list[File] = []
    for path, code, prose in tokens_of(sorted(ROOT.rglob("*.rs")), ROOT, refused):
        owner, role = owner_of(path)
        files.append((path, owner, role, code, prose))
    docs = [p for p in sorted(ROOT.glob("docs/**/*.md")) if "docs/api/" not in str(p)]
    docs += [ROOT / "README.md", ROOT / "CONTRIBUTING.md"]
    for path, code, prose in tokens_of(docs, ROOT, refused):
        files.append((path, "docs", "docs", code, prose))
    for root in extra:
        for path, code, prose in tokens_of(sorted(root.rglob("*.rs")), root, refused):
            files.append((path, root.name, "consumer", code, prose))
    return files


def strip_generics(text: str) -> str:
    """The line with every `<…>` removed, so an item's path survives its
    generics. `->` is an ARROW, not a closing bracket: a bound spelled
    inline (`<L: Fn(&Path) -> Listing>`) would otherwise close the depth one
    character early and let the bound's own text through as part of the path."""
    out, depth, previous = [], 0, ""
    for ch in text:
        if ch == "<":
            depth += 1
        elif ch == ">" and previous != "-":
            depth -= 1
        elif depth == 0:
            out.append(ch)
        previous = ch
    return "".join(out)


Item = tuple[str, str, str, str, str | None]  # crate, path, kind, leaf, parent-type


def items_of(crate: str, lines: list[str]) -> list[Item]:
    """The record's named items: free items and members, without the
    `impl` lines, module lines and derive-generated methods."""
    ident = crate.replace("-", "_")
    out: list[Item] = []
    for line in lines:
        line = ATTR_PREFIX.sub("", line)
        if not line.startswith("pub ") or line.startswith("pub mod ") or "'async_trait" in line:
            continue
        body = line[4:]
        m = re.match(r"(?:(?:async|const|unsafe|extern \"C\") )*(fn|struct|enum|trait|type|const|static|use) (.*)", body)
        kind, rest = (m.group(1), m.group(2)) if m else ("member", body)
        rest = re.split(r"\(| = |: ", strip_generics(rest), maxsplit=1)[0].strip()
        segs = rest.split("::")
        if len(segs) < 2 or segs[0] != ident:
            continue
        leaf, parent = segs[-1], segs[-2]
        if leaf in TRAIT_NOISE:
            continue
        member = kind == "member" or (kind == "fn" and parent[:1].isupper())
        out.append((ident, rest, kind, leaf, parent if member else None))
    return out


def reach(item: Item, files: list[File], loose: bool = False):
    """(who reaches it in CODE, who only mentions it in PROSE)."""
    crate, _path, _kind, leaf, parent = item
    in_code: collections.Counter = collections.Counter()
    in_prose: collections.Counter = collections.Counter()
    for _path2, owner, role, code, prose in files:
        if owner == crate and role == "src":
            continue
        both = code | prose
        where = both if loose else code
        named = crate in where
        mentioned = crate in both
        group = f"{owner}:{role}" if role != "src" else owner
        if named and leaf in where and (parent is None or parent in where):
            in_code[group] += 1
        elif mentioned and leaf in both and (parent is None or parent in both):
            in_prose[group] += 1
    return in_code, in_prose


def census(crates: list[str], extra: list[Path], probe: list[Item] | None = None, loose: bool = False):
    refused: list[str] = []
    files = consumers(extra, refused)
    report = {}
    for crate in crates:
        _stamp, lines = parse(record_path(crate))
        items = items_of(crate, lines) + [p for p in (probe or []) if p[0] == crate.replace("-", "_")]
        report[crate] = [(item, *reach(item, files, loose)) for item in items]
    return report, len(files), refused


def read_from(extra: list[Path]) -> str:
    """What the census read beyond this tree — said before its verdicts, so a
    census with NO consumer tree says what it did not see: an item only the
    embedder calls reads as unreached here, and the compiler against that
    embedder's checkout is what settles it."""
    if extra:
        return ", including " + ", ".join(str(r) for r in extra)
    return (
        " — and NO consumer tree: `--consumer DIR` adds the embedder's checkout,"
        " without which an item only the embedder calls reads as unreached;"
        " `NML_DOWNSTREAM=<embedder checkout> just gate downstream` is the compiler's answer"
    )


def matching(rows: list, needle: str) -> list:
    """The census rows whose item path contains `needle`.

    The record gate names the line that appeared; this is how you ask about
    THAT line without reading the whole census — most of which is the
    language's data model, public whether or not today's consumers read it."""
    return [row for row in rows if needle in row[0][1]]


def unknown_crates(names: list[str]) -> list[str]:
    """The `--crate` names nothing in `docs/api/` records.

    A missing record parses as no items at all, so a typo would print a
    clean `0 named items` and exit 0 — a review tool answering "nobody needs
    anything here" because it read nothing. Refuse instead."""
    return [name for name in names if name not in CRATES]


# One line of every kind the four records carry, and what `items_of` must
# make of it: (path, kind, leaf, parent-type) — or None for a line that is
# not a named item of this crate.
PARSER_CASES: list[tuple[str, tuple[str, str, str, str | None] | None]] = [
    # Not named items: the module line, the `impl` header, a trait impl's.
    ("pub mod nml_validate::fs", None),
    ("impl nml_validate::fs::StdFs", None),
    ("impl core::clone::Clone for nml_validate::fs::EntryKind", None),
    # Free items, one of every kind the records render.
    ("pub struct nml_validate::fs::StdFs", ("nml_validate::fs::StdFs", "struct", "StdFs", None)),
    ("pub enum nml_validate::fs::EntryKind", ("nml_validate::fs::EntryKind", "enum", "EntryKind", None)),
    ("pub trait nml_validate::fs::LstatFs", ("nml_validate::fs::LstatFs", "trait", "LstatFs", None)),
    ("pub const nml_validate::fs::MAX_SOURCE_BYTES: usize",
     ("nml_validate::fs::MAX_SOURCE_BYTES", "const", "MAX_SOURCE_BYTES", None)),
    ("pub static nml_validate::fs::TABLE: usize", ("nml_validate::fs::TABLE", "static", "TABLE", None)),
    ("pub fn nml_validate::fs::read_leaf(&std::path::Path) -> core::result::Result<alloc::string::String, nml_validate::fs::ReadError>",
     ("nml_validate::fs::read_leaf", "fn", "read_leaf", None)),
    # Generics, in the item's own path and all through its signature.
    ("pub fn nml_validate::fs::wasi_fs_through<L: core::ops::Fn(&std::path::Path) -> nml_validate::fs::Listing>(L) -> nml_validate::fs::WasiFs<L>",
     ("nml_validate::fs::wasi_fs_through", "fn", "wasi_fs_through", None)),
    ("pub fn nml_validate::fs::wasi_fs_through<O, I, E>(O) -> nml_validate::fs::WasiFs<impl core::ops::function::Fn(&std::path::Path) -> nml_validate::fs::Listing> where O: core::ops::function::Fn(&std::path::Path) -> core::io::error::Result<I>, I: core::iter::traits::collect::IntoIterator<Item = core::io::error::Result<E>>, E: nml_validate::fs::DirEntryLike",
     ("nml_validate::fs::wasi_fs_through", "fn", "wasi_fs_through", None)),
    ("pub type nml_validate::fs::Listing = core::result::Result<alloc::vec::Vec<(std::ffi::os_str::OsString, nml_validate::fs::EntryKind)>, nml_validate::fs::FsError>",
     ("nml_validate::fs::Listing", "type", "Listing", None)),
    ("pub use nml_validate::workspace::ExternalClass",
     ("nml_validate::workspace::ExternalClass", "use", "ExternalClass", None)),
    # An ATTRIBUTE may start the line, and the item behind it is a real one.
    ("#[non_exhaustive] pub enum nml_validate::fs::FsError",
     ("nml_validate::fs::FsError", "enum", "FsError", None)),
    ("#[repr(u16)] pub enum nml_validate::fs::Step", ("nml_validate::fs::Step", "enum", "Step", None)),
    # Members: a variant, a field, a method, an `async`/`const` method.
    ("pub nml_validate::fs::EntryKind::Dir", ("nml_validate::fs::EntryKind::Dir", "member", "Dir", "EntryKind")),
    ("pub nml_validate::fs::Step::name: alloc::string::String",
     ("nml_validate::fs::Step::name", "member", "name", "Step")),
    ("pub fn nml_validate::fs::StdFs::child(&self, &std::path::Path) -> core::result::Result<(), nml_validate::fs::FsError>",
     ("nml_validate::fs::StdFs::child", "fn", "child", "StdFs")),
    ("pub async fn nml_validate::fs::StdFs::settle(&self) -> bool",
     ("nml_validate::fs::StdFs::settle", "fn", "settle", "StdFs")),
    ("pub const fn nml_validate::fs::Step::is_root(&self) -> bool",
     ("nml_validate::fs::Step::is_root", "fn", "is_root", "Step")),
    # An `'async_trait` desugaring is the macro's, never a reach question.
    ("pub fn nml_validate::fs::LstatFs::lstat<'life0, 'async_trait>(&'life0 self) -> bool", None),
    # Derive and blanket-impl noise.
    ("pub fn nml_validate::fs::EntryKind::clone(&self) -> nml_validate::fs::EntryKind", None),
    ("pub fn nml_validate::fs::EntryKind::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result", None),
    # Another crate's item, rendered inside this record: censused where it lives.
    ("pub fn nml_core::parse(&str) -> nml_core::ast::File", None),
]


def parser_self_test() -> str | None:
    """`items_of` over one line of every kind — the census counts what it
    parses, so a line shape it reads as prose is an item it can never
    report on, in either direction."""
    for line, expect in PARSER_CASES:
        got = items_of("nml-validate", [line])
        if expect is None:
            if got:
                return f"a line that is not a named item was parsed as one: {line!r} -> {got}"
            continue
        if len(got) != 1:
            return f"a named item was dropped: {line!r} -> {got}"
        _crate, path, kind, leaf, parent = got[0]
        if (path, kind, leaf, parent) != expect:
            return f"{line!r} parsed as {(path, kind, leaf, parent)}, expected {expect}"
    named = items_of("nml-validate", [line for line, _ in PARSER_CASES])
    expected = [e for _, e in PARSER_CASES if e is not None]
    if [(i[1], i[2], i[3], i[4]) for i in named] != expected:
        return f"the record read as a whole differs from the lines read one by one: {named}"
    return None


def one_file(owner: str, role: str, code: set[str], prose: set[str] | None = None) -> File:
    return (Path("x.rs"), owner, role, code, prose or set())


def member_rule_self_test() -> str | None:
    """A member counts as reached only where its OWNING TYPE is named in
    the same file: `Dir` alone is a word, `EntryKind` beside it is a use."""
    item: Item = ("nml_validate", "nml_validate::fs::EntryKind::Dir", "member", "Dir", "EntryKind")
    free: Item = ("nml_validate", "nml_validate::fs::read_leaf", "fn", "read_leaf", None)
    cases = [
        ({"nml_validate", "EntryKind", "Dir"}, True, "the type and the member together"),
        ({"nml_validate", "Dir"}, False, "the member's name alone"),
        ({"nml_validate", "EntryKind"}, False, "the type alone"),
        ({"EntryKind", "Dir"}, False, "neither file names the crate"),
    ]
    for toks, expect, why in cases:
        code, _prose = reach(item, [one_file("consumer", "consumer", toks)])
        if bool(code) != expect:
            return f"the member rule is wrong for {why}: {dict(code)}"
    if not reach(free, [one_file("consumer", "consumer", {"nml_validate", "read_leaf"})])[0]:
        return "a FREE item was not reached by a file that names the crate and the item"
    if reach(free, [one_file("nml_validate", "src", {"nml_validate", "read_leaf"})])[0]:
        return "a crate's own `src/` was counted as a consumer of that crate"
    return None


def consumer_spelling_self_test() -> str | None:
    """An extra consumer tree, read from disk, spelled the way a real
    embedder spells it. The MODULE import — `use nml_core::diff;` and then
    `diff::wrap_file_as_body(..)` — is the shape a census that matched whole
    `nml_core::item` paths could not see, and the narrowing it missed broke
    an embedder's build. Tokens, not paths, is what makes this a reach."""
    item: Item = ("nml_core", "nml_core::diff::wrap_file_as_body", "fn", "wrap_file_as_body", None)
    absent: Item = ("nml_core", "nml_core::diff::never_named_anywhere", "fn", "never_named_anywhere", None)
    source = (
        "use nml_core::diff;\n"
        "use std::path::PathBuf;\n"
        "fn wrap(file: &nml_core::ast::File) -> nml_core::ast::Body {\n"
        "    let _ = PathBuf::new();\n"
        "    diff::wrap_file_as_body(file)\n"
        "}\n"
    )
    with tempfile.TemporaryDirectory() as tmp:
        tree = Path(tmp) / "an-embedder"
        (tree / "src").mkdir(parents=True)
        (tree / "src" / "reload.rs").write_text(source, encoding="utf-8")
        refused: list[str] = []
        files = [(path, tree.name, "consumer", code, prose)
                 for path, code, prose in tokens_of(sorted(tree.rglob("*.rs")), tree, refused)]
        if not files:
            return "the consumer tree was not read at all"
        if not reach(item, files)[0]:
            return (
                "a consumer that imports the MODULE and calls the item through it "
                "was not seen as a reach — the census is matching paths, not identifiers"
            )
        if reach(absent, files)[0]:
            return "an item the consumer never names was reported as reached"
    return None


def split_self_test() -> str | None:
    """The census must not be fooled by a sentence, and must not read what a
    consumer tree only pretends is a Rust file."""
    code, prose = rust_split(
        'use nml_core::parse;\n'
        '// nml_core CommentOnly\n'
        '/* nml_core /* nested */ BlockOnly */\n'
        '/// nml_core DocOnly\n'
        'let s = "nml_core StringOnly";\n'
        'let r = r#"nml_core RawOnly"#;\n'
        'let c = \'x\';\n'
        'fn f<\'a>(x: &\'a CodeOnly) -> nml_core::Real { parse(x) }\n'
    )
    got_code = set(IDENT.findall(code))
    got_prose = set(IDENT.findall(prose))
    for name in ("CommentOnly", "BlockOnly", "DocOnly", "StringOnly", "RawOnly"):
        if name in got_code:
            return f"`{name}` is prose, but the code half kept it"
        if name not in got_prose:
            return f"`{name}` is prose, but the prose half lost it"
    for name in ("parse", "CodeOnly", "Real", "nml_core"):
        if name not in got_code:
            return f"`{name}` is code, but the code half lost it"
    if "nested" in got_code:
        return "a NESTED block comment was not closed where it ends"
    if "a" not in got_code:
        return "a lifetime was read as a char literal"
    md_code, md_prose = markdown_split("Prose names MdProse.\n```rust\nuse nml_core::MdCode;\n```\n")
    if "MdCode" not in set(IDENT.findall(md_code)):
        return "a fenced block of a doc page is code, but the code half lost it"
    if "MdProse" in set(IDENT.findall(md_code)):
        return "a doc page's sentences are prose, but the code half kept them"
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "tree"
        root.mkdir()
        outside = Path(tmp) / "outside.rs"
        outside.write_text("nml_core Escaped\n", encoding="utf-8")
        (root / "escape.rs").symlink_to(outside)
        os.mkfifo(root / "hang.rs")
        (root / "big.rs").write_text("x" * (MAX_FILE_BYTES + 1), encoding="utf-8")
        (root / "real.rs").write_text("use nml_core::parse;\n", encoding="utf-8")
        refused: list[str] = []
        seen = tokens_of(sorted(root.rglob("*.rs")), root, refused)
        names = sorted(p.name for p, _c, _pr in seen)
        if names != ["real.rs"]:
            return f"a consumer tree's FIFO, over-bound file or link out was read: {names}"
        if len(refused) != 3:
            return f"every refusal is named; got {refused}"
    return None


def whole_record_self_test() -> str | None:
    """Over the real record: an item nothing names is reported, one every
    front end names is not, `--item` narrows, a crate with no record is
    refused."""
    probe: list[Item] = [("nml_core", "nml_core::zz_api_reach_probe", "fn", "zz_api_reach_probe", None)]
    report, _scanned, _refused = census(["nml-core"], [], probe)
    rows = {item[1]: (code, prose) for item, code, prose in report["nml-core"]}
    if any(rows["nml_core::zz_api_reach_probe"]):
        return "an item nothing names was reported as reached"
    if not rows.get("nml_core::parse", (None, None))[0]:
        return "`nml_core::parse` (named by every front end) was reported as unreached"
    if unknown_crates(["nml-core", "nml-nope"]) != ["nml-nope"]:
        return "a crate with no record was not refused"
    probed = report["nml-core"]
    if [r[0][1] for r in matching(probed, "zz_api_reach_probe")] != ["nml_core::zz_api_reach_probe"] \
            or matching(probed, "zz_no_such_item"):
        return "--item did not narrow to the item asked about"
    if "NO consumer tree" not in read_from([]) or "gate downstream" not in read_from([]):
        return "a census with no consumer tree does not say what it did not read"
    if "NO consumer tree" in read_from([Path("x")]):
        return "a census WITH a consumer tree still says it read none"
    return None


def self_test() -> str | None:
    for name, check in (
        ("the parser", parser_self_test),
        ("the member rule", member_rule_self_test),
        ("a consumer tree", consumer_spelling_self_test),
        ("the code/prose split", split_self_test),
        ("the whole record", whole_record_self_test),
    ):
        verdict = check()
        if verdict is not None:
            return f"{name}: {verdict}"
    return None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("--consumer", action="append", default=[], type=Path, help="an extra tree of Rust consumers (a platform checkout)")
    ap.add_argument("--crate", action="append", default=[], help="restrict to these crates (default: every recorded crate)")
    ap.add_argument("--item", help="only items whose recorded path contains this text, reached or not")
    ap.add_argument("--all", action="store_true", help="print every item with who reaches it")
    ap.add_argument("--loose", action="store_true", help="count a name in a comment or a string as a reach (the wider net)")
    ap.add_argument("--self-test", action="store_true", help="prove the census still sees an unreached item")
    args = ap.parse_args()
    crates = args.crate or CRATES
    if unknown := unknown_crates(args.crate):
        print(
            f"api_reach: no record for {', '.join(unknown)} — the recorded crates are "
            f"{', '.join(CRATES)}",
            file=sys.stderr,
        )
        return 2
    for root in args.consumer:
        if not root.is_dir():
            print(
                f"api_reach: --consumer {root} is not a directory — give it the ROOT of a "
                "checkout that consumes these crates (its `*.rs` files are read, nothing else)",
                file=sys.stderr,
            )
            return 2
    if args.self_test:
        if (broken := self_test()) is not None:
            print(f"api_reach: self-test FAILED — {broken}")
            return 1
        print(
            "api_reach: self-test ok — every record line kind parses, the member rule holds, a "
            "consumer's module import is a reach, a name in prose is not, a consumer tree's FIFO, "
            "over-bound file and link out are refused, an unreached item is reported and a reached "
            "one is not, --item narrows to the item asked about, and a crate with no record is refused"
        )
        return 0
    report, scanned, refused = census(crates, args.consumer, loose=args.loose)
    rule = "a name in a comment or a string counts (--loose)" if args.loose else "code positions only"
    print(f"api_reach: {scanned} consumer files, {rule}" + read_from(args.consumer))
    print(
        "           a name census: a miss only says that no tree read here spells the name in"
        " code. What settles a narrowing is the compiler, against every consumer."
    )
    for why in refused:
        print(f"api_reach: refused {why}", file=sys.stderr)
    matched = 0
    for crate, rows in report.items():
        if args.item:
            rows = matching(rows, args.item)
            if not rows:
                continue
        matched += len(rows)
        unreached = [item for item, code, prose in rows if not code and not prose]
        prose_only = [(item, prose) for item, code, prose in rows if not code and prose]
        test_only = [(item, code) for item, code, _prose in rows
                     if code and all(g.split(":")[-1] in TEST_ROLES for g in code)]
        print(f"\n== {crate}: {len(rows)} named items; {len(unreached)} reached by nothing outside "
              f"the crate; {len(prose_only)} named only in prose; {len(test_only)} only by tests, "
              "fuzz or examples")
        for item in unreached:
            print(f"   unreached   {item[2]:6s} {item[1]}")
        for item, prose in prose_only:
            print(f"   prose-only  {item[2]:6s} {item[1]}  {dict(prose)}")
        for item, code in test_only:
            print(f"   test-only   {item[2]:6s} {item[1]}  {dict(code)}")
        if args.all or args.item:
            for item, code, _prose in rows:
                if code and (item, code) not in test_only:
                    print(f"   reached     {item[2]:6s} {item[1]}  {dict(code)}")
    if args.item and not matched:
        print(
            f"api_reach: no recorded item's path contains {args.item!r} — an item the "
            "record does not carry is not public yet, so there is nothing to ask about",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
