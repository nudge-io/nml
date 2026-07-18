//! RFC 0030 P2 — structural CST editing: a minimal rust-analyzer-`ted`-style
//! splice API over the lossless tree.
//!
//! The one operation tooling needs today is "insert an entry into the block
//! at this path" (the LSP's pin / opt-out writes into `nml-project.nml`). It
//! is implemented
//! as a **green-tree splice** (`rowan`'s mutable-tree API): the source parses
//! to the lossless CST, the new entry parses inside a synthetic wrapper, and
//! the wrapper's parsed elements are moved into the target body with
//! [`SyntaxNode::splice_children`]. Every token outside the insertion —
//! comments, blank lines, exotic indentation — is carried over **byte-for-byte**
//! because it is never re-rendered; only new children are attached. That
//! byte-preservation is the entire point: a hand-maintained project file must
//! survive a machine edit untouched except for the inserted lines.
//!
//! Why splice source-parsed elements instead of building green tokens by hand:
//! `rowan` has no public token factory, and hand-assembled green children would
//! re-encode grammar knowledge (which trivia goes where) that the parser
//! already owns. Parsing a wrapper mints real tokens — indentation
//! `Whitespace`, the entry node, its terminating `Newline` — with exactly the
//! shapes the parser itself produces.

use super::ast::{self, AstNode as _};
use super::{parse, NmlLanguage, SyntaxKind, SyntaxNode};

/// A child element (node or token) of the NML tree.
type SyntaxElement = rowan::SyntaxElement<NmlLanguage>;

/// Where a new entry lands inside the target block's body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPosition {
    /// Immediately before the first existing entry. Comment lines between the
    /// header and that entry stay put (they usually document it), so the new
    /// entry lands after them.
    First,
    /// Immediately after the last existing entry's line. Trailing blank lines
    /// or comments that visually close the block stay below the new entry.
    Last,
    /// Immediately after the block's header line, before everything else in
    /// the body — including any comment lines above the first entry. This is
    /// the pin/opt-out shape: `autoAssociate = false` directly under
    /// `project X:`.
    AfterHeader,
}

/// Insert `entry_snippet` into the body of the block addressed by `path`,
/// returning the complete new document text. `None` when the path does not
/// resolve to exactly one block, the snippet does not parse as one-or-more
/// body entries, or the document's structure defeats the edit.
///
/// * `path` is a root-to-target address whose resolution grammar is
///   **heterogeneous by depth**: the FIRST segment resolves a top-level
///   [`ast::BlockDecl`] by its **keyword** (`"project"` matches
///   `project MyApp:` — the keyword is the stable, machine-known address; the
///   name is user-chosen and unknowable to callers), and each SUBSEQUENT
///   segment resolves an [`ast::NestedBlock`] by **name** among the direct
///   entries of the previously resolved block's body. At EVERY segment, zero
///   matches refuse (`None`) and more than one match refuses too — fail loud
///   rather than guess (house ethos). Refusal-by-construction is the security
///   property: a decoy `schemaPackages:` planted under some *other* block is
///   unreachable because resolution never leaves the addressed parent, and a
///   duplicate under the *right* parent refuses rather than picking one and
///   silently misdirecting the write.
/// * `entry_snippet` is written at **zero indentation** (relative indentation
///   for nested lines): `"- name"`, `"autoAssociate = false"`,
///   `"schemaPackages:\n    - name"`. The target indentation is derived from
///   the block's existing entries (verbatim — odd widths included; tabs are
///   illegal NML indentation, so a tab-indented source is refused via the
///   parse-error gate below — a safe refusal, not verbatim adoption), or
///   header indent + four spaces when the body is empty.
///
/// Everything outside the inserted lines is preserved byte-for-byte.
pub fn insert_entry_at_path(
    source: &str,
    path: &[&str],
    entry_snippet: &str,
    position: EntryPosition,
) -> Option<String> {
    let parsed = parse(source);
    // Error gate: an error-recovered tree is a structural GUESS at the
    // author's intent (probe: "x = = 1"), and splicing relative to guessed
    // node boundaries corrupts the document on write-back. Editing is only
    // safe against a tree the source round-trips through cleanly — refuse
    // everything else (the mirror of `parse_entry_run`'s snippet gate).
    if !parsed.errors().is_empty() {
        return None;
    }
    // `clone_for_update` up front: rowan only permits mutation (splice/attach)
    // on a tree cloned into the mutable representation. All byte offsets are
    // read *before* the single splice, so indexing `source` stays valid.
    let root = parsed.syntax().clone_for_update();
    let block = resolve_path(&root, path)?;
    let body = block.children().find(|n| n.kind() == SyntaxKind::Body);

    let indent = entry_indent(source, &block, body.as_ref());
    let (spare_newline, run) = parse_entry_run(entry_snippet, &indent)?;

    match body {
        Some(body) => insert_into_body(source, &body, run, spare_newline, position)?,
        // A bare `name:` header parses with **no Body node at all** (the
        // terminating newline is the block's sibling), so an "empty body"
        // insert is really an insert next to the block in its parent.
        None => insert_after_bare_header(&block, run, spare_newline)?,
    }

    Some(root.text().to_string())
}

/// Resolve `path` (see [`insert_entry_at_path`]'s grammar) to its unique
/// target block node. Each hop narrows to the resolved block's own [`Body`]
/// before matching the next segment, so nothing outside the addressed lineage
/// is ever a candidate.
fn resolve_path(root: &SyntaxNode, path: &[&str]) -> Option<SyntaxNode> {
    let (first, rest) = path.split_first()?;
    // Depth 0: top-level declarations, addressed by keyword.
    let mut current = unique_match(root.children().filter(|n| {
        ast::BlockDecl::cast(n.clone())
            .and_then(|b| b.keyword())
            .is_some_and(|kw| kw.text() == *first)
    }))?;
    // Depth 1+: nested blocks, addressed by name, within the previous body.
    // A body-less intermediate block simply has zero matches → refuse.
    for segment in rest {
        let body = current.children().find(|n| n.kind() == SyntaxKind::Body)?;
        current = unique_match(body.children().filter(|n| {
            ast::NestedBlock::cast(n.clone())
                .and_then(|b| b.name())
                .is_some_and(|name| name.text() == *segment)
        }))?;
    }
    Some(current)
}

/// Exactly-one gate: `None` for zero matches (nothing to edit) AND for two or
/// more (ambiguous — refuse to guess which sibling the caller meant, since a
/// wrong guess writes into the wrong block).
fn unique_match(mut candidates: impl Iterator<Item = SyntaxNode>) -> Option<SyntaxNode> {
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}

/// Whether `node` is a body entry (any [`ast::Entry`] kind).
fn is_entry(node: &SyntaxNode) -> bool {
    ast::Entry::cast(node.clone()).is_some()
}

/// The indentation string for a new entry in `block`'s body.
///
/// Preference order: the existing first entry's own line indentation
/// (authoritative — whatever width the file uses; tab indentation never
/// reaches here, the parse-error gate in `insert_entry_at_path` refuses
/// it), else the block header's line indentation plus one four-space level
/// (the house style, matching what the LSP historically wrote).
fn entry_indent(source: &str, block: &SyntaxNode, body: Option<&SyntaxNode>) -> String {
    if let Some(entry) = body.and_then(|b| b.children().find(is_entry)) {
        // The CST attaches each entry line's leading Whitespace *inside* the
        // entry node, but a leading attached comment may precede it there, so
        // the reliable sample is the token immediately before the entry's
        // first significant token (e.g. the `-` of a list item).
        let first_significant = entry
            .descendants_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| !t.kind().is_trivia() && t.kind() != SyntaxKind::Indent);
        if let Some(ws) = first_significant
            .and_then(|t| t.prev_token())
            .filter(|t| t.kind() == SyntaxKind::Whitespace)
        {
            return ws.text().to_string();
        }
    }
    // Empty body: header indent + one level. The header's own indent is the
    // whitespace prefix of its line, located via the keyword/name token (the
    // block node itself may *start* with an attached leading comment, so the
    // node's start offset is not the header line's start).
    let header_indent = block
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::Ident)
        .map(|kw| {
            let start = usize::from(kw.text_range().start());
            let line_start = source[..start].rfind('\n').map_or(0, |i| i + 1);
            let prefix = &source[line_start..start];
            if prefix.chars().all(|c| c == ' ' || c == '\t') {
                prefix.to_string()
            } else {
                String::new()
            }
        })
        .unwrap_or_default();
    format!("{header_indent}    ")
}

/// Parse `entry_snippet` (re-indented to `indent`) inside a synthetic wrapper
/// block and return `(spare_newline, run)`:
///
/// * `run` — the detached-ready elements forming the entry's complete source
///   lines: leading `Whitespace` lives inside each entry node, a property/item
///   entry is followed by its terminating `Newline` sibling, and a nested
///   block carries its newline inside its own body.
/// * `spare_newline` — one extra `Newline` token (the wrapper header's), for
///   callers that must first repair a missing line terminator at the insertion
///   point (a file ending without `\n`).
///
/// `None` when the snippet does not parse cleanly as one-or-more entries — the
/// structural analogue of injection safety: text that would change meaning
/// beyond adding entries cannot come out of this function.
fn parse_entry_run(
    entry_snippet: &str,
    indent: &str,
) -> Option<(SyntaxElement, Vec<SyntaxElement>)> {
    // `w W:` — a top-level block header needs both keyword and name; the
    // snippet's lines become its body.
    let mut wrapper = String::from("w W:\n");
    for line in entry_snippet.lines() {
        if line.trim().is_empty() {
            wrapper.push('\n');
        } else {
            wrapper.push_str(indent);
            wrapper.push_str(line);
            wrapper.push('\n');
        }
    }
    let parsed = parse(&wrapper);
    if !parsed.errors().is_empty() {
        return None;
    }
    // Mutable clone: elements can only be attached to another tree when they
    // come from a mutable tree themselves (`attach_child` detaches them from
    // this wrapper on insert).
    let wrapper_root = parsed.syntax().clone_for_update();
    let body = wrapper_root
        .descendants()
        .find(|n| n.kind() == SyntaxKind::Body)?;
    let children: Vec<SyntaxElement> = body.children_with_tokens().collect();
    // The wrapper body reads: Newline (header terminator), Indent, <run>,
    // Dedent. Everything strictly between the layout markers is the run —
    // entry nodes plus their sibling newlines, never the zero-width markers
    // themselves (a reparse of the edited text regenerates those).
    let first_indent = children
        .iter()
        .position(|e| e.kind() == SyntaxKind::Indent)?;
    let last_dedent = children
        .iter()
        .rposition(|e| e.kind() == SyntaxKind::Dedent)?;
    if last_dedent <= first_indent {
        return None;
    }
    let run: Vec<SyntaxElement> = children[first_indent + 1..last_dedent].to_vec();
    if !run.iter().any(|e| e.as_node().is_some_and(is_entry)) {
        return None;
    }
    let spare_newline = children
        .iter()
        .find(|e| e.kind() == SyntaxKind::Newline)?
        .clone();
    Some((spare_newline, run))
}

/// Splice `run` into `body` at `position`. All offset reads happen against the
/// pre-edit `source`; the splice is the single mutation.
fn insert_into_body(
    source: &str,
    body: &SyntaxNode,
    run: Vec<SyntaxElement>,
    spare_newline: SyntaxElement,
    position: EntryPosition,
) -> Option<()> {
    let children: Vec<SyntaxElement> = body.children_with_tokens().collect();
    // Splice indices are `children_with_tokens` element positions.
    let before_dedent = || {
        children
            .iter()
            .rposition(|e| e.kind() == SyntaxKind::Dedent)
            .unwrap_or(children.len())
    };
    let index = match position {
        EntryPosition::AfterHeader => children
            .iter()
            .position(|e| e.kind() == SyntaxKind::Newline)
            .map_or(0, |i| i + 1),
        EntryPosition::First => children
            .iter()
            .position(|e| e.as_node().is_some_and(is_entry))
            .unwrap_or_else(before_dedent),
        // Right after the last entry's line — NOT before the body's closing
        // Dedent, which would land the new entry below any trailing blank
        // lines/comments that visually separate this block from the next.
        EntryPosition::Last => match children
            .iter()
            .rposition(|e| e.as_node().is_some_and(is_entry))
        {
            // A property/list-item entry is terminated by a sibling Newline
            // (skip past it); a nested-block entry carries its newline inside
            // its own body, so the entry itself already ends the line. The
            // terminator is located by skipping trivia first: under CRLF the
            // `\r` lexes as a Whitespace token sitting between the entry and
            // its Newline, and `\r`+Newline is ONE terminator — landing
            // between them would split the pair and spuriously trip the
            // missing-terminator repair below (the offset before a bare `\r`
            // does not end with '\n').
            Some(i) => {
                let mut after = i + 1;
                while children
                    .get(after)
                    .is_some_and(|e| e.kind() == SyntaxKind::Whitespace)
                {
                    after += 1;
                }
                if children
                    .get(after)
                    .is_some_and(|e| e.kind() == SyntaxKind::Newline)
                {
                    after + 1
                } else {
                    i + 1
                }
            }
            None => before_dedent(),
        },
    };
    // Line-terminator repair: inserting "…entry\n" only yields well-formed
    // lines when the insertion point itself sits at a line start. The one case
    // it does not is a body whose last line lacks a trailing newline (EOF
    // without `\n`, before the zero-width Dedent) — prepend the spare Newline
    // so the existing last line is terminated first.
    let insert_offset = children.get(index).map_or_else(
        || usize::from(body.text_range().end()),
        |e| usize::from(e.text_range().start()),
    );
    let mut elements = run;
    if insert_offset > 0 && !source[..insert_offset].ends_with('\n') {
        elements.insert(0, spare_newline);
    }
    body.splice_children(index..index, elements);
    Some(())
}

/// Insert next to a body-less `name:` header. The header's terminating
/// `Newline` is a sibling of the block node in its parent, so the entry lines
/// go right after it — producing exactly the text a parse-with-body would
/// have had, without hand-building a `Body` node (the reparse creates it).
fn insert_after_bare_header(
    block: &SyntaxNode,
    run: Vec<SyntaxElement>,
    spare_newline: SyntaxElement,
) -> Option<()> {
    // Only a real header (with its colon) is a block we can give a body.
    block
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::Colon)?;
    let parent = block.parent()?;
    let children: Vec<SyntaxElement> = parent.children_with_tokens().collect();
    let block_index = children.iter().position(|e| e.as_node() == Some(block))?;
    let after_block = children.get(block_index + 1);
    let (index, elements) = if after_block.is_some_and(|e| e.kind() == SyntaxKind::Newline) {
        // `name:\n` — insert after the existing terminator.
        (block_index + 2, run)
    } else {
        // `name:` at EOF (no newline) — terminate the header line first.
        let mut with_newline = vec![spare_newline];
        with_newline.extend(run);
        (block_index + 1, with_newline)
    };
    parent.splice_children(index..index, elements);
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cst::parse_to_ast_all;
    use crate::project::ProjectConfig;

    /// Reparse `text`, assert it is fully valid, and return the pinned package
    /// names — the property-style check that the edit produced meaningful
    /// structure, not merely plausible text.
    fn reparse_pins(text: &str) -> Vec<String> {
        let (file, errors) = parse_to_ast_all(text);
        assert!(
            errors.is_empty(),
            "edited text must reparse clean: {errors:?}\n---\n{text}"
        );
        ProjectConfig::from_file(&file).schema_packages
    }

    #[test]
    fn insert_last_preserves_comments_and_blank_lines_byte_for_byte() {
        // (a) Existing entries with interleaved comments and a blank line: the
        // result must be the source with ONLY the new line added — every
        // comment/blank-line byte outside the insertion survives verbatim.
        let src = "\
// header comment
project MyApp:
    // why we pin
    schemaPackages:
        - alpha
        // beta has a story
        - beta

    autoAssociate = false
";
        let expected = "\
// header comment
project MyApp:
    // why we pin
    schemaPackages:
        - alpha
        // beta has a story
        - beta
        - gamma

    autoAssociate = false
";
        let out = insert_entry_at_path(
            src,
            &["project", "schemaPackages"],
            "- gamma",
            EntryPosition::Last,
        )
        .expect("insert succeeds");
        assert_eq!(out, expected);
        assert_eq!(reparse_pins(&out), ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn insert_first_lands_before_existing_entries() {
        let src = "project P:\n    schemaPackages:\n        // docs for alpha\n        - alpha\n";
        let out = insert_entry_at_path(
            src,
            &["project", "schemaPackages"],
            "- zero",
            EntryPosition::First,
        )
        .expect("insert succeeds");
        // First = before the first entry NODE; the comment above it stays with
        // the block header region (it may document the block, not the entry).
        assert_eq!(
            out,
            "project P:\n    schemaPackages:\n        // docs for alpha\n        - zero\n        - alpha\n"
        );
        assert_eq!(reparse_pins(&out), ["zero", "alpha"]);
    }

    #[test]
    fn insert_after_header_precedes_body_comments() {
        let src = "project P:\n    // a note about pins\n    schemaPackages:\n        - a\n";
        let out = insert_entry_at_path(
            src,
            &["project"],
            "autoAssociate = false",
            EntryPosition::AfterHeader,
        )
        .expect("insert succeeds");
        assert_eq!(
            out,
            "project P:\n    autoAssociate = false\n    // a note about pins\n    schemaPackages:\n        - a\n"
        );
        reparse_pins(&out);
    }

    #[test]
    fn insert_into_empty_block_body() {
        // (b) A bare header has no Body node at all — the edit must still
        // produce a correctly indented body, with and without a trailing
        // newline in the source.
        let out = insert_entry_at_path(
            "project MyApp:\n",
            &["project"],
            "autoAssociate = false",
            EntryPosition::AfterHeader,
        )
        .expect("insert succeeds");
        assert_eq!(out, "project MyApp:\n    autoAssociate = false\n");
        reparse_pins(&out);

        let out = insert_entry_at_path(
            "project MyApp:",
            &["project"],
            "autoAssociate = false",
            EntryPosition::Last,
        )
        .expect("insert succeeds");
        assert_eq!(out, "project MyApp:\n    autoAssociate = false\n");
        reparse_pins(&out);
    }

    #[test]
    fn path_not_found_returns_none() {
        // (c) No matching block at some segment — zero matches refuse.
        let src = "project P:\n    autoAssociate = false\n";
        // `project` resolves but has no nested `schemaPackages`.
        assert_eq!(
            insert_entry_at_path(
                src,
                &["project", "schemaPackages"],
                "- x",
                EntryPosition::Last
            ),
            None
        );
        // No top-level block with keyword `nope`.
        assert_eq!(
            insert_entry_at_path(src, &["nope"], "- x", EntryPosition::Last),
            None
        );
        // The first segment addresses by KEYWORD, never by user-chosen name.
        assert_eq!(
            insert_entry_at_path(src, &["P"], "- x", EntryPosition::Last),
            None
        );
        // Empty document and empty path are both refusals, not panics.
        assert_eq!(
            insert_entry_at_path("", &["project"], "- x", EntryPosition::Last),
            None
        );
        assert_eq!(
            insert_entry_at_path(src, &[], "- x", EntryPosition::Last),
            None
        );
    }

    #[test]
    fn decoy_nested_block_under_sibling_is_unreachable() {
        // A `schemaPackages:` planted under a DIFFERENT top-level block must
        // never receive a pin addressed to project's: resolution descends only
        // through the addressed parent, so the decoy is not a candidate at
        // all. With project owning a real block, the pin lands there.
        let src = "\
service S:
    schemaPackages:
        - decoy
project P:
    schemaPackages:
        - real
";
        let out = insert_entry_at_path(
            src,
            &["project", "schemaPackages"],
            "- pinned",
            EntryPosition::Last,
        )
        .expect("insert succeeds");
        assert_eq!(
            out,
            "\
service S:
    schemaPackages:
        - decoy
project P:
    schemaPackages:
        - real
        - pinned
"
        );

        // With NO schemaPackages under project, the decoy must not be found
        // as a fallback — the path refuses (the LSP caller then creates the
        // block under project via the [\"project\"] path instead).
        let src = "service S:\n    schemaPackages:\n        - decoy\nproject P:\n    x = 1\n";
        assert_eq!(
            insert_entry_at_path(
                src,
                &["project", "schemaPackages"],
                "- pinned",
                EntryPosition::Last
            ),
            None
        );
    }

    #[test]
    fn ambiguous_segments_refuse() {
        // TWO top-level `project` blocks: which one the caller means is a
        // guess — refuse at the first segment.
        let src = "project A:\n    x = 1\nproject B:\n    y = 2\n";
        assert_eq!(
            insert_entry_at_path(src, &["project"], "z = 3", EntryPosition::Last),
            None
        );

        // Duplicate `schemaPackages:` under the addressed project: writing
        // into either one silently misdirects — refuse at the second segment.
        let src = "\
project P:
    schemaPackages:
        - a
    schemaPackages:
        - b
";
        assert_eq!(
            insert_entry_at_path(
                src,
                &["project", "schemaPackages"],
                "- c",
                EntryPosition::Last
            ),
            None
        );
    }

    #[test]
    fn nested_block_insert_then_second_item_into_it() {
        // (d) Create `schemaPackages:` (with its first item) via one call,
        // then target THAT nested block on a second call.
        let src = "project P:\n    autoAssociate = false\n";
        let step1 = insert_entry_at_path(
            src,
            &["project"],
            "schemaPackages:\n    - first",
            EntryPosition::AfterHeader,
        )
        .expect("nested block insert succeeds");
        assert_eq!(
            step1,
            "project P:\n    schemaPackages:\n        - first\n    autoAssociate = false\n"
        );
        assert_eq!(reparse_pins(&step1), ["first"]);

        let step2 = insert_entry_at_path(
            &step1,
            &["project", "schemaPackages"],
            "- second",
            EntryPosition::Last,
        )
        .expect("second item insert succeeds");
        assert_eq!(
            step2,
            "project P:\n    schemaPackages:\n        - first\n        - second\n    autoAssociate = false\n"
        );
        assert_eq!(reparse_pins(&step2), ["first", "second"]);
    }

    #[test]
    fn indentation_is_adopted_from_existing_entries_verbatim() {
        // Two-space file: the new item copies the existing item's exact
        // indentation, not the four-space house default.
        let src = "project P:\n  schemaPackages:\n    - a\n";
        let out = insert_entry_at_path(
            src,
            &["project", "schemaPackages"],
            "- b",
            EntryPosition::Last,
        )
        .expect("insert succeeds");
        assert_eq!(out, "project P:\n  schemaPackages:\n    - a\n    - b\n");
        assert_eq!(reparse_pins(&out), ["a", "b"]);

        // Empty nested body in a two-space file: header indent + one level.
        let src = "project P:\n  autoAssociate = false\n";
        let out = insert_entry_at_path(
            src,
            &["project"],
            "schemaPackages:\n    - a",
            EntryPosition::AfterHeader,
        )
        .expect("insert succeeds");
        assert_eq!(
            out,
            "project P:\n  schemaPackages:\n      - a\n  autoAssociate = false\n"
        );
        assert_eq!(reparse_pins(&out), ["a"]);
    }

    #[test]
    fn missing_trailing_newline_is_repaired_before_appending() {
        // The file's last line has no `\n`; appending must terminate it first
        // instead of gluing the new entry onto it.
        let src = "project P:\n    x = 1";
        let out = insert_entry_at_path(src, &["project"], "y = 2", EntryPosition::Last)
            .expect("insert succeeds");
        assert_eq!(out, "project P:\n    x = 1\n    y = 2\n");
        reparse_pins(&out);
    }

    #[test]
    fn crlf_terminator_is_treated_as_one_unit() {
        // CRLF sources lex `\r` as a Whitespace token BETWEEN the entry and
        // its Newline. The Last-position insert must land after the full
        // `\r\n` pair — not between `\r` and `\n`, where the spurious
        // missing-terminator repair used to fire and mangle the line.
        let src = "project P:\r\n    x = 1\r\n";
        let out = insert_entry_at_path(src, &["project"], "y = 2", EntryPosition::Last)
            .expect("insert succeeds");
        // Exact bytes: existing CRLF lines are untouched; the inserted entry
        // (minted by the LF-only wrapper parse) lands on its own line after
        // the intact `\r\n` terminator.
        assert_eq!(out, "project P:\r\n    x = 1\r\n    y = 2\n");
        reparse_pins(&out);
    }

    #[test]
    fn source_with_parse_errors_is_refused() {
        // Splicing into an error-recovered tree corrupts it (the recovered
        // structure is a guess); the edit must refuse outright.
        let src = "project P:\n    x = = 1\n";
        assert_eq!(
            insert_entry_at_path(src, &["project"], "y = 2", EntryPosition::Last),
            None
        );
        // Tab indentation is a lexer diagnostic, so it rides the same gate.
        let tabbed = "project P:\n\tx = 1\n";
        assert_eq!(
            insert_entry_at_path(tabbed, &["project"], "y = 2", EntryPosition::Last),
            None
        );
    }

    #[test]
    fn snippet_that_is_not_a_valid_entry_returns_none() {
        // Structural injection safety: a snippet that does not parse cleanly
        // as body entries is refused outright, never spliced as text.
        let src = "project P:\n    x = 1\n";
        assert_eq!(
            insert_entry_at_path(src, &["project"], "@@@ nonsense", EntryPosition::Last),
            None
        );
        assert_eq!(
            insert_entry_at_path(src, &["project"], "", EntryPosition::Last),
            None
        );
    }
}
