//! Structural CST editing over the lossless tree — TWO operations with
//! deliberately different mechanics:
//!
//! * **Insertion** (RFC 0030 P2, [`insert_entry_at_path`] — the LSP's pin /
//!   opt-out writes into `nml-project.nml`) is a **green-tree splice**
//!   (`rowan`'s mutable-tree API): the source parses to the lossless CST,
//!   the new entry parses inside a synthetic wrapper, and the wrapper's
//!   parsed elements are moved into the target body with
//!   [`SyntaxNode::splice_children`]. Every token outside the insertion —
//!   comments, blank lines, exotic indentation — is carried over
//!   **byte-for-byte** because it is never re-rendered; only new children
//!   are attached. Insertion MINTS tokens, and `rowan` has no public token
//!   factory: hand-assembled green children would re-encode grammar
//!   knowledge (which trivia goes where) that the parser already owns, so
//!   parsing a wrapper mints real tokens with exactly the shapes the parser
//!   itself produces.
//! * **Deletion** (RFC 0023 Part A, [`resolve_suggestions`]) is **range
//!   computation over the token stream — token walks, never tree
//!   mutation**. Deletion mints no tokens, and node detachment is wrong
//!   here by construction: the parser flushes leading trivia into the node
//!   that opens next (RFC 0004 §4.3), so an entry's terminating newline is
//!   the NEXT entry's leading trivia, the header's newline is the `Body`'s
//!   first token, and detaching a node swallows its neighbour's line
//!   break. The tree-mutation engines (Roslyn's `SyntaxEditor.RemoveNode`,
//!   Biome's `BatchMutation::remove_node`, rust-analyzer's `ted::remove`)
//!   each carry an explicit trivia policy because detachment alone
//!   mis-assigns line breaks; the walks express that policy without a
//!   mutable clone — and purely textual removal (clang-tidy's
//!   `FixItHint::CreateRemoval`, ESLint's `fixer.remove`) produces exactly
//!   the blank-line wart the walks exist to avoid.
//!
//! Refusals are owned HERE, per suggestion, for every applier (`nml fix`
//! and the editor's quick-fix): the structural-injection guard, the
//! parse-dirty gate for structural edits, stale spans, shared-line and
//! `.shared`-block shapes, batch overlap. A silent pre-filter in one
//! applier would be an exemption from "every refusal is printed".

use super::ast::{self, AstNode as _};
use super::syntax::content_span;
use super::{NmlLanguage, SyntaxKind, SyntaxNode, SyntaxToken, parse};
use crate::diagnostic::{Suggestion, SuggestionKind};
use crate::span::Span;

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

// ── Span splicing (RFC 0017 §4.1) ─────────────────────────────────────────

/// One byte-range replacement for [`splice`] — the applier shape of a
/// machine-applicable [`Suggestion`](crate::diagnostic::Suggestion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpliceEdit {
    /// The exact byte range the replacement substitutes.
    pub span: crate::span::Span,
    /// The text spliced in at `span` (empty = deletion).
    pub replacement: String,
}

/// Why [`splice`] refused. Refusal is total — a splice that could apply
/// *some* of its edits would leave the file in a state no diagnostic
/// described.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpliceError {
    #[error("edit span {start}..{end} is out of bounds (source is {len} bytes)")]
    OutOfBounds {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("edit spans {a_start}..{a_end} and {b_start}..{b_end} overlap")]
    Overlap {
        a_start: usize,
        a_end: usize,
        b_start: usize,
        b_end: usize,
    },
    #[error("edit span {start}..{end} does not fall on character boundaries")]
    NotCharBoundary { start: usize, end: usize },
}

/// Apply byte-span replacements to `source` in one pass — the span-splice
/// primitive `nml fix` applies suggestions with (RFC 0017 §4.1). Edits
/// are applied **highest-offset-first**, so earlier edits can never
/// invalidate later spans; input order is irrelevant. Overlapping,
/// out-of-bounds, or non-boundary spans refuse the whole batch
/// ([`SpliceError`]) — the caller filters candidates, this validates as
/// the last line of defense.
pub fn splice(source: &str, edits: &[SpliceEdit]) -> Result<String, SpliceError> {
    let mut ordered: Vec<&SpliceEdit> = edits.iter().collect();
    ordered.sort_by_key(|e| (e.span.start, e.span.end));
    validate_edits(source, &ordered)?;
    let mut out = source.to_string();
    for e in ordered.iter().rev() {
        out.replace_range(e.span.start..e.span.end, &e.replacement);
    }
    Ok(out)
}

/// The shared edit check — the ONE owner of the overlap predicate
/// (strict, so adjacent edits are legal), bounds, and char boundaries.
/// [`splice`] runs it as the applier's last line of defense;
/// [`resolve_suggestions`] asserts it over its own output (the resolver
/// mints edits the applier never saw — a colon drop, a separator — and
/// its batch rules must keep them splice-ready). `edits` must be sorted
/// by `(start, end)`.
fn validate_edits(source: &str, edits: &[&SpliceEdit]) -> Result<(), SpliceError> {
    for pair in edits.windows(2) {
        if pair[1].span.start < pair[0].span.end {
            return Err(SpliceError::Overlap {
                a_start: pair[0].span.start,
                a_end: pair[0].span.end,
                b_start: pair[1].span.start,
                b_end: pair[1].span.end,
            });
        }
    }
    for e in edits {
        let (start, end) = (e.span.start, e.span.end);
        if start > end || end > source.len() {
            return Err(SpliceError::OutOfBounds {
                start,
                end,
                len: source.len(),
            });
        }
        if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
            return Err(SpliceError::NotCharBoundary { start, end });
        }
    }
    Ok(())
}

// ── Structural fix resolution (RFC 0023 Part A) ───────────────────────────

/// Why one suggestion was refused. Every refusal is per-suggestion —
/// nothing refuses a batch: even a source that does not parse refuses
/// each STRUCTURAL suggestion alone, because the parse-layer fixes
/// (`=>` → `->`, the misaligned closing quote) are how a file *becomes*
/// parseable. `Display` carries no offsets — the printer adds `line:col`
/// from [`Self::span`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SuggestionError {
    /// The replacement embeds a line break or another control character —
    /// some replacements carry decoded user content, and a crafted escape
    /// must never smuggle new *structure* through an auto-applied fix.
    /// One guard, every applier.
    #[error("the replacement contains a control character")]
    ControlCharacter { span: Span },
    /// A structural deletion against a source that does not parse: an
    /// error-recovered tree is a guess at the author's intent, and
    /// deleting relative to guessed boundaries corrupts the document.
    #[error("a structural deletion needs a source that parses ({errors} error(s))")]
    UnparsableSource { span: Span, errors: usize },
    /// No entry, clause, or clause reference has exactly this content
    /// span — a stale suggestion fails closed instead of editing a stale
    /// offset.
    #[error("no deletable node at this span")]
    NoNodeAt { span: Span },
    /// The grammar accepts several entries on one line
    /// (`host = "h"    port = 1` parses as two properties); deleting line
    /// rows there would take a sibling. The formatter never prints that
    /// shape — a total rule exists and is deferred.
    #[error("the node shares its line with another entry")]
    NotLineExclusive { span: Span },
    /// The target lies inside a `.shared` block's body. A shared row is
    /// DISTRIBUTED into every named item, not owned by the one item
    /// whose diagnostic suggested the deletion — removing it would
    /// silently change every other item's value (and removing the last
    /// row would empty the block, which the lowerer re-reads as a
    /// scalar and the formatter prints `.x = ""`). Refused whole; only
    /// deleting the entire `.shared` entry (or a container above it)
    /// removes shared rows.
    #[error("the entry is distributed by its `.shared` block into every named item")]
    SharedDistribution { span: Span },
    /// A verbatim suggestion's span is not a valid byte range of this
    /// source (out of bounds, inverted, or off a char boundary) — a
    /// stale or hostile span fails closed here, per suggestion, instead
    /// of poisoning the batch at the splice. (A structural suggestion's
    /// bad span is `NoNodeAt`.)
    #[error("the span is not a valid byte range of the source")]
    InvalidSpan { span: Span },
    /// This suggestion's bytes overlap an earlier accepted suggestion's
    /// (`with` names that suggestion) — deferred, the applier's own
    /// overlap doctrine; the next round re-derives it against the
    /// updated text.
    #[error("overlaps an earlier suggestion's edit (deferred to a later round)")]
    Overlap { span: Span, with: usize },
}

impl SuggestionError {
    /// The refused suggestion's span — the printer's anchor.
    pub fn span(&self) -> Span {
        match self {
            SuggestionError::ControlCharacter { span }
            | SuggestionError::UnparsableSource { span, .. }
            | SuggestionError::NoNodeAt { span }
            | SuggestionError::NotLineExclusive { span }
            | SuggestionError::SharedDistribution { span }
            | SuggestionError::InvalidSpan { span }
            | SuggestionError::Overlap { span, .. } => *span,
        }
    }
}

/// What a structural deletion removed — closed, so every title exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    Property,
    NestedBlock,
    ListItem,
    Modifier,
    SharedProperty,
    Arm,
    FieldDef,
    UsesClause,
    LayerRef,
}

impl Deleted {
    /// The editor's quick-fix title.
    pub fn title(self) -> &'static str {
        match self {
            Deleted::Property => "Delete this property",
            Deleted::NestedBlock => "Delete this nested block",
            Deleted::ListItem => "Delete this list item",
            Deleted::Modifier => "Delete this modifier",
            Deleted::SharedProperty => "Delete this shared property",
            Deleted::Arm => "Delete this arm",
            Deleted::FieldDef => "Delete this field definition",
            Deleted::UsesClause => "Delete the `uses` clause",
            Deleted::LayerRef => "Remove this layer reference",
        }
    }
}

/// [`resolve_suggestions`]' result.
pub struct Resolved {
    /// Sorted, non-overlapping (adjacent allowed) — [`splice`]-ready.
    pub edits: Vec<SpliceEdit>,
    /// One outcome per input suggestion, in order: a verbatim edit
    /// (`Ok(None)`), a structural deletion (`Ok(Some(_))` — a target
    /// subsumed by a containing deletion included, with no edits of its
    /// own), or a refusal.
    pub outcomes: Vec<Result<Option<Deleted>, SuggestionError>>,
}

/// One suggestion resolved alone — phase 1 of [`resolve_suggestions`],
/// before any batch rule.
enum PerSuggestion {
    Verbatim(SpliceEdit),
    Deletion {
        kind: Deleted,
        edits: Vec<SpliceEdit>,
        /// The target node's (or token's) full byte range — subsumption
        /// containment is judged on targets, not on computed rows.
        node: (usize, usize),
        /// The owning `Body` and the entry node — the emptied-body pass
        /// (entry targets only).
        entry_of: Option<(SyntaxNode, SyntaxNode)>,
        /// The `Uses` node this target removes outright (a clause
        /// deletion, or the only reference) — clause bookkeeping for the
        /// colon rule.
        uses_of: Option<SyntaxNode>,
        /// The target lies inside a `.shared` block's body — refused
        /// (`SharedDistribution`) unless a containing deletion subsumes
        /// it (deleting the whole shared block is legitimate; deleting
        /// one distributed row on one item's diagnostic is not).
        distributed: bool,
    },
}

/// ONE owner for both appliers (`nml fix` and the editor's quick-fix):
/// verbatim substitution for did-you-mean and mechanical fixes — with
/// the structural-injection refusal, so no applier is exempt — and
/// structural expansion for [`SuggestionKind::Delete`], computed from
/// the lossless tree by **token walks, never tree mutation** (see the
/// module doc for why detachment is wrong here).
///
/// Suggestions are processed in the given order (the batch applier
/// sorts by suggestion span, so a containing target precedes what it
/// subsumes; the editor passes singletons); a later suggestion whose
/// bytes overlap an earlier accepted one's is refused (`Overlap`),
/// never silently dropped. The tree is parsed once, lazily, on the
/// first `Delete`. Targets are located by recomputed content span over
/// three classes — entry nodes, `Uses` clause nodes, and
/// clause-reference `Ident` tokens (never the `uses` keyword, itself an
/// `Ident` inside the clause) — equality required, so a stale span
/// fails closed (`NoNodeAt`). Bounds and char boundaries of the
/// resulting batch are what the applier's [`splice`] validates — the
/// shared last line of defense.
pub fn resolve_suggestions(source: &str, suggestions: &[Suggestion]) -> Resolved {
    // Parsed once, lazily: only a `Delete` needs the tree, and a parse
    // error must never refuse a verbatim suggestion.
    let tree: Option<Result<SyntaxNode, usize>> = suggestions
        .iter()
        .any(|s| s.kind == SuggestionKind::Delete)
        .then(|| {
            let parsed = parse(source);
            match parsed.errors().len() {
                0 => Ok(parsed.syntax()),
                n => Err(n),
            }
        });

    // Phase 1: each suggestion alone.
    let per: Vec<Result<PerSuggestion, SuggestionError>> = suggestions
        .iter()
        .map(|s| match s.kind {
            SuggestionKind::Delete => match &tree {
                Some(Ok(root)) => resolve_delete(source, root, s.span),
                Some(Err(errors)) => Err(SuggestionError::UnparsableSource {
                    span: s.span,
                    errors: *errors,
                }),
                None => unreachable!("a Delete forced the parse"),
            },
            SuggestionKind::DidYouMean | SuggestionKind::Fix => {
                // The refusal set IS the render-escape set
                // (`needs_escape`): controls, the Trojan-Source bidi
                // set, U+2028/U+2029 — the editor path has no re-check
                // gate, so this guard is its sole injection defense and
                // must not be narrower than the source policy.
                if s.replacement.chars().any(crate::diagnostic::needs_escape) {
                    Err(SuggestionError::ControlCharacter { span: s.span })
                } else if s.span.start > s.span.end
                    || s.span.end > source.len()
                    || !source.is_char_boundary(s.span.start)
                    || !source.is_char_boundary(s.span.end)
                {
                    // Caller-supplied bytes fail closed HERE, per
                    // suggestion — `Resolved.edits` stays splice-ready
                    // by construction, never poisoned for the batch.
                    Err(SuggestionError::InvalidSpan { span: s.span })
                } else {
                    Ok(PerSuggestion::Verbatim(SpliceEdit {
                        span: s.span,
                        replacement: s.replacement.clone(),
                    }))
                }
            }
        })
        .collect();

    // Phase 2: batch rules, greedily in the given order — subsumption,
    // the `.shared` distribution refusal, overlap, acceptance.
    let mut outcomes: Vec<Result<Option<Deleted>, SuggestionError>> = Vec::new();
    let mut edits: Vec<SpliceEdit> = Vec::new();
    let mut accepted: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, suggestion)
    let mut accepted_nodes: Vec<(usize, usize)> = Vec::new();
    let mut deleted_entries: Vec<(SyntaxNode, SyntaxNode)> = Vec::new();
    let mut deleted_uses: Vec<SyntaxNode> = Vec::new();
    let overlap = |accepted: &[(usize, usize, usize)], span: Span| {
        accepted
            .iter()
            .find(|&&(s, e, _)| s < span.end && span.start < e)
            .map(|&(_, _, i)| i)
    };
    for (i, r) in per.into_iter().enumerate() {
        match r {
            Err(e) => outcomes.push(Err(e)),
            Ok(PerSuggestion::Verbatim(edit)) => match overlap(&accepted, edit.span) {
                Some(with) => outcomes.push(Err(SuggestionError::Overlap {
                    span: suggestions[i].span,
                    with,
                })),
                None => {
                    accepted.push((edit.span.start, edit.span.end, i));
                    edits.push(edit);
                    outcomes.push(Ok(None));
                }
            },
            Ok(PerSuggestion::Deletion {
                kind,
                edits: own,
                node,
                entry_of,
                uses_of,
                distributed,
            }) => {
                // Subsumed by an earlier accepted deletion (an entry
                // inside a deleted block — a distributed row inside a
                // deleted `.shared` block included): applied, no edits
                // of its own — never printed as a refusal.
                if accepted_nodes
                    .iter()
                    .any(|&(s, e)| s <= node.0 && node.1 <= e)
                {
                    if let Some(pair) = entry_of {
                        deleted_entries.push(pair);
                    }
                    if let Some(u) = uses_of {
                        deleted_uses.push(u);
                    }
                    outcomes.push(Ok(Some(kind)));
                    continue;
                }
                // A row inside a `.shared` block is distributed into
                // every named item — one item's diagnostic must never
                // rewrite its siblings' values (the block-form `.shared`
                // NML2060 deletion silently stripped a shared default
                // from every item; the scalar form fails `NoNodeAt`).
                if distributed {
                    outcomes.push(Err(SuggestionError::SharedDistribution {
                        span: suggestions[i].span,
                    }));
                    continue;
                }
                if let Some(with) = own.iter().find_map(|e| overlap(&accepted, e.span)) {
                    outcomes.push(Err(SuggestionError::Overlap {
                        span: suggestions[i].span,
                        with,
                    }));
                    continue;
                }
                for e in &own {
                    accepted.push((e.span.start, e.span.end, i));
                }
                accepted_nodes.push(node);
                if let Some(pair) = entry_of {
                    deleted_entries.push(pair);
                }
                if let Some(u) = uses_of {
                    deleted_uses.push(u);
                }
                edits.extend(own);
                outcomes.push(Ok(Some(kind)));
            }
        }
    }

    // Phase 3 — emptied bodies: when the batch's ACCEPTED deletions cover
    // every entry of a body whose owner is a top-level `BlockDecl` still
    // carrying an `is`/`uses` clause after the batch, one splice drops
    // the colon (the formatter's canonical bodyless form for such
    // headers). Every other owner — a plain block, a nested block, a
    // named item, a modifier block, an arm target — keeps its colon: the
    // formatter prints one there, and dropping it would make the fixed
    // file fmt-dirty.
    let mut bodies: Vec<(SyntaxNode, Vec<SyntaxNode>)> = Vec::new();
    for (body, entry) in deleted_entries {
        match bodies.iter_mut().find(|(b, _)| *b == body) {
            Some((_, v)) => v.push(entry),
            None => bodies.push((body, vec![entry])),
        }
    }
    let mut colon_spans: Vec<usize> = Vec::new();
    for (body, deleted) in bodies {
        if !body
            .children()
            .filter(is_entry)
            .all(|e| deleted.contains(&e))
        {
            continue;
        }
        let Some(owner) = body.parent() else { continue };
        // An owner that is itself deleted needs no colon bookkeeping —
        // its whole header is gone with it.
        let o = node_bytes(&owner);
        if accepted_nodes.iter().any(|&(s, e)| s <= o.0 && o.1 <= e) {
            continue;
        }
        if ast::BlockDecl::cast(owner.clone()).is_none() {
            continue;
        }
        let has_is = owner.children().any(|n| n.kind() == SyntaxKind::Extends);
        let uses_remaining = owner
            .children()
            .any(|n| n.kind() == SyntaxKind::Uses && !deleted_uses.contains(&n));
        if !(has_is || uses_remaining) {
            continue;
        }
        let colon = owner
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::Colon);
        if let Some(colon) = colon {
            let at = usize::from(colon.text_range().start());
            // A body emptied by several targets yields its splice ONCE —
            // and never over a byte an accepted suggestion already owns
            // (a verbatim edit at the colon): the deletion still lands,
            // the colon stays, and the batch stays splice-ready.
            if !colon_spans.contains(&at) && overlap(&accepted, Span::new(at, at + 1)).is_none() {
                colon_spans.push(at);
                edits.push(SpliceEdit {
                    span: Span::new(at, at + 1),
                    replacement: String::new(),
                });
            }
        }
    }
    edits.sort_by_key(|e| (e.span.start, e.span.end));
    // Sound unconditionally: verbatim spans are validated per suggestion
    // (`InvalidSpan`), structural edits are token-exact, the greedy pass
    // refuses overlap, and the colon splice checks `accepted` — so every
    // batch's output satisfies the shared edit check by construction.
    debug_assert!(
        validate_edits(source, &edits.iter().collect::<Vec<_>>()).is_ok(),
        "resolved edits must satisfy the shared edit check"
    );
    Resolved { edits, outcomes }
}

/// A node's full byte range (leading trivia included — an entry's
/// leading trivia is what the parser flushed into it).
fn node_bytes(node: &SyntaxNode) -> (usize, usize) {
    let r = node.text_range();
    (usize::from(r.start()), usize::from(r.end()))
}

/// Locate and expand one structural deletion — the three target classes,
/// in order: entry nodes, `Uses` clause nodes, clause-reference `Ident`
/// tokens.
fn resolve_delete(
    source: &str,
    root: &SyntaxNode,
    span: Span,
) -> Result<PerSuggestion, SuggestionError> {
    for node in root.descendants() {
        if let Some(kind) = entry_deleted_kind(&node) {
            if content_span(&node) == span {
                let edits = entry_rows(source, &node, span)?;
                let body = node.parent().filter(|p| p.kind() == SyntaxKind::Body);
                // Strictly ABOVE the entry (`skip(1)`): the `.shared`
                // entry itself stays a legitimate target; its rows are
                // the distributed ones.
                let distributed = node
                    .ancestors()
                    .skip(1)
                    .any(|a| a.kind() == SyntaxKind::SharedProperty);
                return Ok(PerSuggestion::Deletion {
                    kind,
                    edits,
                    node: node_bytes(&node),
                    entry_of: body.map(|b| (b, node.clone())),
                    uses_of: None,
                    distributed,
                });
            }
        } else if node.kind() == SyntaxKind::Uses {
            if content_span(&node) == span {
                return Ok(clause_deletion(&node, Deleted::UsesClause));
            }
            let Some(uses) = ast::Uses::cast(node.clone()) else {
                continue;
            };
            let refs: Vec<SyntaxToken> = uses.refs().collect();
            let Some(idx) = refs.iter().position(|t| {
                let r = t.text_range();
                Span::new(usize::from(r.start()), usize::from(r.end())) == span
            }) else {
                continue;
            };
            if refs.len() == 1 {
                // Deleting the only reference is a clause deletion.
                return Ok(clause_deletion(&node, Deleted::LayerRef));
            }
            // A non-first ref takes the separator BEFORE it; the first
            // ref takes the separator after it.
            let (start, end) = if idx == 0 {
                (
                    usize::from(refs[0].text_range().start()),
                    usize::from(refs[1].text_range().start()),
                )
            } else {
                (
                    usize::from(refs[idx - 1].text_range().end()),
                    usize::from(refs[idx].text_range().end()),
                )
            };
            let r = refs[idx].text_range();
            return Ok(PerSuggestion::Deletion {
                kind: Deleted::LayerRef,
                edits: vec![SpliceEdit {
                    span: Span::new(start, end),
                    replacement: String::new(),
                }],
                node: (usize::from(r.start()), usize::from(r.end())),
                entry_of: None,
                uses_of: None,
                distributed: false,
            });
        }
    }
    Err(SuggestionError::NoNodeAt { span })
}

/// The `Uses` node's own text range — it owns the whitespace separating
/// it from the header (` uses a, b`) — deleted outright; on a bodyless
/// header whose LAST clause this removes, the replacement is `:` (the
/// formatter prints the colon on a bodyless plain header, and drops it
/// only while an `is`/`uses` clause remains).
fn clause_deletion(uses_node: &SyntaxNode, kind: Deleted) -> PerSuggestion {
    let range = node_bytes(uses_node);
    let keeps_shape = uses_node.parent().is_some_and(|block| {
        block
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .any(|t| t.kind() == SyntaxKind::Colon)
            || block.children().any(|n| n.kind() == SyntaxKind::Extends)
    });
    let replacement = if keeps_shape {
        String::new()
    } else {
        ":".to_string()
    };
    PerSuggestion::Deletion {
        kind,
        edits: vec![SpliceEdit {
            span: Span::new(range.0, range.1),
            replacement,
        }],
        node: range,
        entry_of: None,
        uses_of: Some(uses_node.clone()),
        distributed: false,
    }
}

/// The [`Deleted`] kind of an entry node, `None` for every other node.
fn entry_deleted_kind(node: &SyntaxNode) -> Option<Deleted> {
    Some(match ast::Entry::cast(node.clone())? {
        ast::Entry::Property(_) => Deleted::Property,
        ast::Entry::NestedBlock(_) => Deleted::NestedBlock,
        ast::Entry::ListItem(_) => Deleted::ListItem,
        ast::Entry::Modifier(_) => Deleted::Modifier,
        ast::Entry::SharedProperty(_) => Deleted::SharedProperty,
        ast::Entry::FieldDef(_) => Deleted::FieldDef,
        ast::Entry::Arm(_) => Deleted::Arm,
    })
}

/// The range walks' skip set. Deliberately DIFFERENT from
/// `content_span`'s trivia set: the zero-width layout markers are walked
/// through here, and a `Newline` or `Comment` is a stop, never skipped.
fn walk_skip(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Whitespace | SyntaxKind::Indent | SyntaxKind::Dedent
    )
}

/// "Significant" for the walks: neither the skip set nor a stop token.
fn walk_significant(t: &SyntaxToken) -> bool {
    !walk_skip(t.kind()) && !matches!(t.kind(), SyntaxKind::Newline | SyntaxKind::Comment)
}

/// A body entry's deletion rows — two token walks from the entry's first
/// and last significant tokens, no text predicates.
///
/// Backward through the skip set: a `Newline` (or the stream start)
/// starts the row after it — an own-line comment ABOVE the entry is that
/// comment's own line and stays; any other token refuses
/// (`NotLineExclusive`). Forward: a `Newline` ends the row after it (a
/// nested block's zero-width `Dedent` — and the whitespace an outer
/// deferred comment left before it — survives; a block-form value's
/// interior newlines live inside its `String` token); a `Comment` keeps
/// the line, deleting only the entry's own bytes so the indentation, the
/// comment and its newline stay; the stream end takes the tail (EOF
/// without a newline keeps the header's newline). CRLF needs no rule:
/// the `\r` is a `Whitespace` token before the `Newline`. Blank lines
/// after a target stay — the walk stops at the first newline.
fn entry_rows(
    source: &str,
    node: &SyntaxNode,
    at: Span,
) -> Result<Vec<SpliceEdit>, SuggestionError> {
    let mut significant = node
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(walk_significant);
    let Some(first) = significant.next() else {
        return Err(SuggestionError::NoNodeAt { span: at });
    };
    let last = significant.last().unwrap_or_else(|| first.clone());

    let mut tok = first.prev_token();
    let start = loop {
        match tok {
            None => break 0,
            Some(t) if walk_skip(t.kind()) => tok = t.prev_token(),
            Some(t) if t.kind() == SyntaxKind::Newline => break usize::from(t.text_range().end()),
            Some(_) => return Err(SuggestionError::NotLineExclusive { span: at }),
        }
    };
    let mut tok = last.next_token();
    let span = loop {
        match tok {
            None => break Span::new(start, source.len()),
            Some(t) if walk_skip(t.kind()) => tok = t.next_token(),
            Some(t) if t.kind() == SyntaxKind::Newline => {
                break Span::new(start, usize::from(t.text_range().end()));
            }
            Some(t) if t.kind() == SyntaxKind::Comment => {
                break Span::new(
                    usize::from(first.text_range().start()),
                    usize::from(t.text_range().start()),
                );
            }
            Some(_) => return Err(SuggestionError::NotLineExclusive { span: at }),
        }
    };
    Ok(vec![SpliceEdit {
        span,
        replacement: String::new(),
    }])
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

    // ── splice (RFC 0017 §4.1) ────────────────────────────────────────────

    fn edit(start: usize, end: usize, replacement: &str) -> SpliceEdit {
        SpliceEdit {
            span: crate::span::Span::new(start, end),
            replacement: replacement.to_string(),
        }
    }

    #[test]
    fn splice_applies_highest_offset_first_regardless_of_input_order() {
        // Two replacements of different widths: applying low-first would
        // shift the second span; the primitive must be order-independent.
        let src = "a = \"30s\"\nb = \"5m\"\n";
        let edits = [edit(4, 9, "30s"), edit(14, 18, "5m")];
        let expected = "a = 30s\nb = 5m\n";
        assert_eq!(splice(src, &edits).unwrap(), expected);
        let reversed = [edit(14, 18, "5m"), edit(4, 9, "30s")];
        assert_eq!(splice(src, &reversed).unwrap(), expected);
    }

    #[test]
    fn splice_supports_deletions_insertions_and_empty_batches() {
        assert_eq!(splice("abc", &[]).unwrap(), "abc");
        assert_eq!(splice("abc", &[edit(1, 2, "")]).unwrap(), "ac");
        assert_eq!(splice("ac", &[edit(1, 1, "b")]).unwrap(), "abc");
    }

    #[test]
    fn splice_refuses_bad_batches_totally() {
        // Overlap refuses the WHOLE batch — partial application would
        // leave a state no diagnostic described.
        assert!(matches!(
            splice("abcdef", &[edit(0, 3, "x"), edit(2, 4, "y")]),
            Err(SpliceError::Overlap { .. })
        ));
        assert!(matches!(
            splice("abc", &[edit(1, 9, "x")]),
            Err(SpliceError::OutOfBounds { .. })
        ));
        // Mid-UTF-8 spans refuse rather than panic.
        assert!(matches!(
            splice("é", &[edit(1, 2, "x")]),
            Err(SpliceError::NotCharBoundary { .. })
        ));
        // Touching-but-not-overlapping edits are legal.
        assert_eq!(
            splice("abcd", &[edit(0, 2, "X"), edit(2, 4, "Y")]).unwrap(),
            "XY"
        );
    }

    // ── resolve_suggestions (RFC 0023 Part A) ────────────────────────────

    use crate::diagnostic::{Suggestion, SuggestionKind};

    /// Every `BodyEntry` span in `src`, recursively — exactly the spans
    /// producers emit (`content_span`, recorded by the lowerer).
    fn entry_spans(src: &str) -> Vec<Span> {
        fn walk(body: &crate::ast::Body, out: &mut Vec<Span>) {
            for e in &body.entries {
                out.push(e.span);
                match &e.kind {
                    crate::ast::BodyEntryKind::NestedBlock(nb) => walk(&nb.body, out),
                    crate::ast::BodyEntryKind::ListItem(li) => {
                        if let crate::ast::ListItemKind::Named { body, .. } = &li.kind {
                            walk(body, out);
                        }
                    }
                    crate::ast::BodyEntryKind::Arm(arm) => {
                        if let crate::ast::ArmTarget::Inline { body, .. } = &arm.target {
                            walk(body, out);
                        }
                    }
                    crate::ast::BodyEntryKind::Modifier(m) => {
                        if let crate::ast::ModifierValue::Block(items) = &m.value {
                            for li in items {
                                out.push(li.span);
                                if let crate::ast::ListItemKind::Named { body, .. } = &li.kind {
                                    walk(body, out);
                                }
                            }
                        }
                    }
                    crate::ast::BodyEntryKind::SharedProperty(sp) => {
                        if let crate::ast::SharedPropertyKind::Block(body) = &sp.kind {
                            walk(body, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        let (file, errors) = parse_to_ast_all(src);
        assert!(errors.is_empty(), "fixture must parse: {errors:?}");
        let mut out = Vec::new();
        for d in &file.declarations {
            if let crate::ast::DeclarationKind::Block(b) = &d.kind {
                walk(&b.body, &mut out);
            }
        }
        out
    }

    /// The entry whose content span STARTS at `needle`'s first occurrence.
    fn span_at(src: &str, needle: &str) -> Span {
        let at = src
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in source"));
        entry_spans(src)
            .into_iter()
            .find(|s| s.start == at)
            .unwrap_or_else(|| panic!("no entry starts at {needle:?}"))
    }

    fn del(span: Span) -> Suggestion {
        Suggestion {
            replacement: String::new(),
            span,
            kind: SuggestionKind::Delete,
        }
    }

    /// Resolve, assert every outcome applied, splice.
    fn apply(src: &str, suggestions: &[Suggestion]) -> String {
        let r = resolve_suggestions(src, suggestions);
        for (i, o) in r.outcomes.iter().enumerate() {
            assert!(o.is_ok(), "suggestion {i} refused: {o:?}");
        }
        splice(src, &r.edits).expect("splice-ready edits")
    }

    #[test]
    fn deletion_rows_cover_first_middle_and_last_entries() {
        let src = "service Api:\n    a = 1\n    b = 2\n    c = 3\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "a = 1"))]),
            "service Api:\n    b = 2\n    c = 3\n"
        );
        assert_eq!(
            apply(src, &[del(span_at(src, "b = 2"))]),
            "service Api:\n    a = 1\n    c = 3\n"
        );
        assert_eq!(
            apply(src, &[del(span_at(src, "c = 3"))]),
            "service Api:\n    a = 1\n    b = 2\n"
        );
    }

    #[test]
    fn deletion_keeps_a_trailing_comment_at_the_indentation() {
        let src = "service Api:\n    x = 1  // keep me\n    y = 2\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "x = 1"))]),
            "service Api:\n    // keep me\n    y = 2\n"
        );
    }

    #[test]
    fn deletion_keeps_an_own_line_comment_above() {
        // RFC 0004 §4.3 attaches an own-line comment as the FOLLOWING
        // node's leading trivia; the walks never delete prose the author
        // placed before something.
        let src = "service Api:\n    // why x exists\n    x = 1\n    y = 2\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "x = 1"))]),
            "service Api:\n    // why x exists\n    y = 2\n"
        );
    }

    #[test]
    fn a_trailing_own_line_comment_inside_a_deleted_block_survives() {
        // RFC 0004 §4.3 attaches an own-line comment to the FOLLOWING
        // node, so the last comment inside a block is, structurally, the
        // next outer entry's leading trivia — deleting the block leaves
        // it, at its authored indentation. Deliberate: prose an author
        // wrote is never deleted on structural grounds, and the result
        // reparses (fmt may re-home the indentation later).
        let src = "service Api:\n    db:\n        host = \"h\"\n        // last note\n    y = 1\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "db:"))]),
            "service Api:\n        // last note\n    y = 1\n"
        );
    }

    #[test]
    fn nested_block_deletion_keeps_a_deferred_outer_comment() {
        // The block ends `Newline Whitespace Dedent`: the whitespace is
        // the outer comment's indentation and survives.
        let src = "service Api:\n    db:\n        host = \"h\"\n    // outer\n    y = 1\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "db:"))]),
            "service Api:\n    // outer\n    y = 1\n"
        );
    }

    #[test]
    fn deletion_rows_handle_crlf_and_eof_without_newline() {
        let crlf = "service Api:\r\n    x = 1\r\n    y = 2\r\n";
        assert_eq!(
            apply(crlf, &[del(span_at(crlf, "x = 1"))]),
            "service Api:\r\n    y = 2\r\n",
            "the CR is a Whitespace token before the Newline — no rule needed"
        );
        let eof = "service Api:\n    x = 1";
        assert_eq!(
            apply(eof, &[del(span_at(eof, "x = 1"))]),
            "service Api:\n",
            "EOF without a newline keeps the header's newline"
        );
    }

    #[test]
    fn block_form_value_deletion_takes_the_whole_literal() {
        // A block-form value's interior newlines are INSIDE its String
        // token; its terminator is the next entry's leading trivia.
        let src = "service Api:\n    note = \"\"\"\n        text\n        \"\"\"\n    y = 1\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "note"))]),
            "service Api:\n    y = 1\n"
        );
    }

    #[test]
    fn two_entries_on_one_line_refuse_line_rows() {
        // The grammar accepts several entries on one line; deleting line
        // rows there would take a sibling.
        let src = "service Api:\n    a = 1    b = 2\n";
        let r = resolve_suggestions(src, &[del(span_at(src, "a = 1"))]);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::NotLineExclusive { .. })]
            ),
            "{:?}",
            r.outcomes
        );
        assert!(r.edits.is_empty());
    }

    #[test]
    fn emptying_a_clause_carrying_header_drops_the_colon() {
        // The formatter's canonical bodyless form for an `is`/`uses`
        // header has no colon; the resolver adds the one splice.
        let src = "flow t uses base:\n    x = 1\n";
        assert_eq!(
            apply(src, &[del(span_at(src, "x = 1"))]),
            "flow t uses base\n"
        );
    }

    #[test]
    fn emptying_other_owners_keeps_their_colons() {
        // A plain top-level block, a nested block, a modifier block, an
        // arm target: the formatter prints the colon there.
        let plain = "service Api:\n    x = 1\n";
        assert_eq!(
            apply(plain, &[del(span_at(plain, "x = 1"))]),
            "service Api:\n"
        );
        let nested = "service Api:\n    db:\n        h = 1\n    y = 2\n";
        assert_eq!(
            apply(nested, &[del(span_at(nested, "h = 1"))]),
            "service Api:\n    db:\n    y = 2\n"
        );
        let modifier = "service Api:\n    |tags:\n        - a\n    y = 1\n";
        assert_eq!(
            apply(modifier, &[del(span_at(modifier, "- a"))]),
            "service Api:\n    |tags:\n    y = 1\n"
        );
        let list = "service Api:\n    tags:\n        - a\n    y = 1\n";
        assert_eq!(
            apply(list, &[del(span_at(list, "- a"))]),
            "service Api:\n    tags:\n    y = 1\n",
            "an emptied list block keeps its colon"
        );
        let arm = "service Api:\n    routing:\n        \"plan\" -> page:\n            label = 4\n";
        assert_eq!(
            apply(arm, &[del(span_at(arm, "label = 4"))]),
            "service Api:\n    routing:\n        \"plan\" -> page:\n"
        );
    }

    #[test]
    fn a_shared_blocks_rows_are_never_deleted_singly() {
        // A `.shared` row is DISTRIBUTED into every named item; deleting
        // it on ONE item's diagnostic would silently rewrite the others
        // (and deleting the last row would empty the block into a
        // scalar re-spelling). Refused whole — partial or full.
        let last = "service Api:\n    .retry:\n        max = 3\n";
        let r = resolve_suggestions(last, &[del(span_at(last, "max = 3"))]);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::SharedDistribution { .. })]
            ),
            "{:?}",
            r.outcomes
        );
        assert!(r.edits.is_empty());

        // The block-form corruption shape: a sibling row must not make
        // the target deletable — the deletion would strip the shared
        // default from every item, not just the diagnosed one.
        let partial = "service Api:\n    .retry:\n        max = 3\n        mode = \"fast\"\n";
        let r = resolve_suggestions(partial, &[del(span_at(partial, "max = 3"))]);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::SharedDistribution { .. })]
            ),
            "a partial deletion is the corruption vector: {:?}",
            r.outcomes
        );
        assert!(r.edits.is_empty());

        // Nested inside the shared body: still distributed.
        let nested = "service Api:\n    .retry:\n        inner:\n            max = 3\n";
        let r = resolve_suggestions(nested, &[del(span_at(nested, "max = 3"))]);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::SharedDistribution { .. })]
            ),
            "{:?}",
            r.outcomes
        );
    }

    #[test]
    fn deleting_the_whole_shared_entry_subsumes_its_rows() {
        // The `.shared` ENTRY itself is a legitimate target, and a row
        // targeted alongside it is subsumed — applied, never printed as
        // a spurious refusal.
        let src = "service Api:\n    .retry:\n        max = 3\n    y = 1\n";
        let suggestions = [del(span_at(src, ".retry:")), del(span_at(src, "max = 3"))];
        let r = resolve_suggestions(src, &suggestions);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [
                    Ok(Some(Deleted::SharedProperty)),
                    Ok(Some(Deleted::Property))
                ]
            ),
            "{:?}",
            r.outcomes
        );
        assert_eq!(splice(src, &r.edits).unwrap(), "service Api:\n    y = 1\n");
    }

    #[test]
    fn two_targets_emptying_one_body_yield_one_colon_splice() {
        let src = "flow t uses base:\n    a = 1\n    b = 2\n";
        let suggestions = [del(span_at(src, "a = 1")), del(span_at(src, "b = 2"))];
        let r = resolve_suggestions(src, &suggestions);
        assert!(r.outcomes.iter().all(|o| o.is_ok()), "{:?}", r.outcomes);
        assert_eq!(
            splice(src, &r.edits).unwrap(),
            "flow t uses base\n",
            "both rows plus exactly one colon splice"
        );
    }

    #[test]
    fn an_entry_inside_a_deleted_block_is_subsumed() {
        let src = "service Api:\n    db:\n        host = \"h\"\n    y = 1\n";
        let suggestions = [del(span_at(src, "db:")), del(span_at(src, "host"))];
        let r = resolve_suggestions(src, &suggestions);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Ok(Some(Deleted::NestedBlock)), Ok(Some(Deleted::Property))]
            ),
            "subsumed, never refused: {:?}",
            r.outcomes
        );
        assert_eq!(
            splice(src, &r.edits).unwrap(),
            "service Api:\n    y = 1\n",
            "the container's rows only"
        );
    }

    #[test]
    fn a_later_overlapping_suggestion_is_refused_with_its_earlier_index() {
        // A deletion containing a value-level verbatim fix: the container
        // (sorted first) lands, the moot inner fix defers.
        let src = "service Api:\n    x = 999\n    y = 1\n";
        let inner = src.find("999").unwrap();
        let suggestions = [
            del(span_at(src, "x = 999")),
            Suggestion {
                replacement: "1".into(),
                span: Span::new(inner, inner + 3),
                kind: SuggestionKind::Fix,
            },
        ];
        let r = resolve_suggestions(src, &suggestions);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [
                    Ok(Some(Deleted::Property)),
                    Err(SuggestionError::Overlap { with: 0, .. })
                ]
            ),
            "{:?}",
            r.outcomes
        );
        assert_eq!(splice(src, &r.edits).unwrap(), "service Api:\n    y = 1\n");
    }

    #[test]
    fn a_stale_span_fails_closed() {
        let src = "service Api:\n    x = 1\n";
        let r = resolve_suggestions(src, &[del(Span::new(3, 9))]);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::NoNodeAt { .. })]
            ),
            "{:?}",
            r.outcomes
        );
    }

    #[test]
    fn a_parse_dirty_source_refuses_each_delete_and_no_verbatim_fix() {
        // The parse-layer fixes are how a file BECOMES parseable — a
        // batch refusal would disable them.
        let src = "service Api:\n    x = = 1\n    kind => y\n";
        let arrow = src.find("=>").unwrap();
        let suggestions = [
            del(Span::new(17, 22)),
            Suggestion {
                replacement: "->".into(),
                span: Span::new(arrow, arrow + 2),
                kind: SuggestionKind::Fix,
            },
        ];
        let r = resolve_suggestions(src, &suggestions);
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::UnparsableSource { .. }), Ok(None)]
            ),
            "{:?}",
            r.outcomes
        );
        assert_eq!(r.edits.len(), 1, "the verbatim fix still applies");
    }

    #[test]
    fn a_control_character_replacement_is_refused() {
        // THE injection guard — one owner, every applier: decoded user
        // content must never smuggle new structure through a fix.
        let src = "service Api:\n    owner = \"a\"\n";
        let at = src.find("\"a\"").unwrap() + 1;
        let r = resolve_suggestions(
            src,
            &[Suggestion {
                replacement: "admin\n    evil = 1".into(),
                span: Span::new(at, at + 1),
                kind: SuggestionKind::DidYouMean,
            }],
        );
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::ControlCharacter { .. })]
            ),
            "{:?}",
            r.outcomes
        );
        assert!(r.edits.is_empty());
        // The refusal set IS the source policy's set (plus every
        // control): the Trojan-Source bidi overrides and the Unicode
        // line/paragraph separators are refused via `must_escape`.
        for hostile in ['\u{202E}', '\u{2066}', '\u{FEFF}', '\u{2028}', '\u{2029}'] {
            let r = resolve_suggestions(
                src,
                &[Suggestion {
                    replacement: format!("a{hostile}b"),
                    span: Span::new(at, at + 1),
                    kind: SuggestionKind::Fix,
                }],
            );
            assert!(
                matches!(
                    r.outcomes.as_slice(),
                    [Err(SuggestionError::ControlCharacter { .. })]
                ),
                "U+{:04X} must be refused: {:?}",
                hostile as u32,
                r.outcomes
            );
        }
    }

    /// The `uses` clause span the NML2062 producer emits (the lowered
    /// `BlockDecl.uses_span`).
    fn clause_span(src: &str) -> Span {
        let (file, errors) = parse_to_ast_all(src);
        assert!(errors.is_empty(), "{errors:?}");
        file.declarations
            .iter()
            .find_map(|d| match &d.kind {
                crate::ast::DeclarationKind::Block(b) => b.uses_span,
                _ => None,
            })
            .expect("a uses clause")
    }

    /// The layer-ref spans the NML2077 producer emits.
    fn ref_span(src: &str, name: &str) -> Span {
        let (file, errors) = parse_to_ast_all(src);
        assert!(errors.is_empty(), "{errors:?}");
        file.declarations
            .iter()
            .find_map(|d| match &d.kind {
                crate::ast::DeclarationKind::Block(b) => {
                    b.uses.iter().find(|u| u.name == name).map(|u| u.span)
                }
                _ => None,
            })
            .expect("the ref")
    }

    #[test]
    fn clause_deletion_with_and_without_a_body() {
        let with_body = "model n uses m:\n    x = 1\n";
        assert_eq!(
            apply(with_body, &[del(clause_span(with_body))]),
            "model n:\n    x = 1\n"
        );
        let bodyless = "flow t uses base\n";
        assert_eq!(
            apply(bodyless, &[del(clause_span(bodyless))]),
            "flow t:\n",
            "a bodyless plain header gets its colon back"
        );
        let with_is = "flow t is p uses a\n";
        assert_eq!(
            apply(with_is, &[del(clause_span(with_is))]),
            "flow t is p\n",
            "an `is` clause keeps the colon off"
        );
    }

    #[test]
    fn ref_deletion_takes_its_separator_and_the_only_ref_takes_the_clause() {
        let src = "flow t uses a, b:\n    x = 1\n";
        assert_eq!(
            apply(src, &[del(ref_span(src, "b"))]),
            "flow t uses a:\n    x = 1\n",
            "a non-first ref takes the separator before it"
        );
        assert_eq!(
            apply(src, &[del(ref_span(src, "a"))]),
            "flow t uses b:\n    x = 1\n",
            "the first ref takes the separator after it"
        );
        let only = "flow t uses a\n";
        let r = resolve_suggestions(only, &[del(ref_span(only, "a"))]);
        assert!(
            matches!(r.outcomes.as_slice(), [Ok(Some(Deleted::LayerRef))]),
            "{:?}",
            r.outcomes
        );
        assert_eq!(
            splice(only, &r.edits).unwrap(),
            "flow t:\n",
            "the only ref is a clause deletion, colon restored"
        );
    }

    #[test]
    fn invalid_verbatim_spans_fail_closed_per_suggestion() {
        // The three constructed debug-assert trips from certification:
        // an out-of-bounds verbatim span, a mid-UTF-8-boundary span, and
        // a colon-byte verbatim beside a body-emptying deletion. Each
        // now resolves to a valid batch: the bad spans refuse
        // per-suggestion (`InvalidSpan`), and the colon splice is never
        // minted over an accepted edit.
        let src = "flow t uses base:\n    x = 1\n";
        let r = resolve_suggestions(
            src,
            &[
                del(span_at(src, "x = 1")),
                Suggestion {
                    replacement: "y".into(),
                    span: Span::new(9_999, 10_000),
                    kind: SuggestionKind::DidYouMean,
                },
            ],
        );
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [
                    Ok(Some(Deleted::Property)),
                    Err(SuggestionError::InvalidSpan { .. })
                ]
            ),
            "{:?}",
            r.outcomes
        );
        assert_eq!(splice(src, &r.edits).unwrap(), "flow t uses base\n");

        let inverted = resolve_suggestions(
            src,
            &[Suggestion {
                replacement: "y".into(),
                span: Span::new(5, 3),
                kind: SuggestionKind::Fix,
            }],
        );
        assert!(
            matches!(
                inverted.outcomes.as_slice(),
                [Err(SuggestionError::InvalidSpan { .. })]
            ),
            "an inverted span fails closed: {:?}",
            inverted.outcomes
        );

        let uni = "service Api:\n    x = \"é\"\n";
        let mid = uni.find('é').unwrap() + 1; // inside the two-byte char
        let r = resolve_suggestions(
            uni,
            &[Suggestion {
                replacement: "e".into(),
                span: Span::new(mid, mid + 1),
                kind: SuggestionKind::Fix,
            }],
        );
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Err(SuggestionError::InvalidSpan { .. })]
            ),
            "{:?}",
            r.outcomes
        );

        // A verbatim edit that owns the header's colon byte: the
        // emptying deletion still lands, the colon splice is skipped,
        // and the batch stays splice-ready.
        let colon_at = src.find(':').unwrap();
        let r = resolve_suggestions(
            src,
            &[
                Suggestion {
                    replacement: ";".into(),
                    span: Span::new(colon_at, colon_at + 1),
                    kind: SuggestionKind::Fix,
                },
                del(span_at(src, "x = 1")),
            ],
        );
        assert!(
            matches!(
                r.outcomes.as_slice(),
                [Ok(None), Ok(Some(Deleted::Property))]
            ),
            "{:?}",
            r.outcomes
        );
        assert_eq!(
            splice(src, &r.edits).unwrap(),
            "flow t uses base;\n",
            "the colon byte belongs to the verbatim edit; no second splice"
        );
    }

    #[test]
    fn an_empty_verbatim_fix_stays_a_byte_removal() {
        // `Fix("")` is NumberTrailingDot's verbatim removal — never
        // widened, never structural.
        let src = "service Api:\n    x = 1299.\n";
        let dot = src.find("1299.").unwrap() + 4;
        let r = resolve_suggestions(
            src,
            &[Suggestion {
                replacement: String::new(),
                span: Span::new(dot, dot + 1),
                kind: SuggestionKind::Fix,
            }],
        );
        assert!(
            matches!(r.outcomes.as_slice(), [Ok(None)]),
            "{:?}",
            r.outcomes
        );
        assert_eq!(
            splice(src, &r.edits).unwrap(),
            "service Api:\n    x = 1299\n"
        );
    }

    #[test]
    fn deleted_titles_are_total_and_distinct() {
        let all = [
            Deleted::Property,
            Deleted::NestedBlock,
            Deleted::ListItem,
            Deleted::Modifier,
            Deleted::SharedProperty,
            Deleted::Arm,
            Deleted::FieldDef,
            Deleted::UsesClause,
            Deleted::LayerRef,
        ];
        let titles: Vec<&str> = all.iter().map(|d| d.title()).collect();
        for t in &titles {
            assert!(!t.is_empty());
        }
        let unique: std::collections::HashSet<&&str> = titles.iter().collect();
        assert_eq!(unique.len(), titles.len(), "{titles:?}");
    }
}
