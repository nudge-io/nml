//! The line printer: output assembly and the blank-line policy.
//!
//! The formatter emits **logical lines**. A logical line is a run of
//! significant tokens plus an optional trailing comment; it carries a depth,
//! and its indentation is generated, never copied. Between two lines the
//! printer emits the author's blank run, capped by [`Blanks`] — the one
//! place the blank-line policy lives (`spec/style.md` §2).
//!
//! Keeping assembly here, away from the tree walk, is what makes the policy
//! auditable: a blank line is decided by two numbers (the run the author
//! wrote, and the depth of the line about to be written) and by nothing
//! else.

use nml_core::cst::INDENT_UNIT;

/// The blank-line budget (`spec/style.md` §2). Two between top-level
/// declarations, one inside a block — PEP 8's split, which every NML file in
/// this repository independently arrived at, and which gofmt's single cap
/// tightens only because Go has no top-level grouping idiom.
pub(crate) struct Blanks;

impl Blanks {
    /// Between top-level declarations.
    const TOP: usize = 2;
    /// Inside any block body.
    const NESTED: usize = 1;

    /// The budget for a line at `depth`.
    ///
    /// UNPUBLISHED: a style constant, not a resource bound: it shapes output, refuses nothing
    const fn cap(depth: usize) -> usize {
        if depth == 0 { Self::TOP } else { Self::NESTED }
    }
}

/// Assembles the output one logical line at a time.
pub(crate) struct Printer {
    out: String,
    /// The line under construction, WITHOUT its leading indentation.
    line: String,
    /// The depth the current line will be written at.
    line_depth: usize,
    /// Blank lines the author wrote since the last flush.
    pending_blanks: usize,
    /// The depth of the last line written, or `None` before the first.
    flushed_depth: Option<usize>,
}

impl Printer {
    pub(crate) fn new() -> Self {
        Self {
            out: String::new(),
            line: String::new(),
            line_depth: 0,
            pending_blanks: 0,
            flushed_depth: None,
        }
    }

    /// True while nothing has been written to the line under construction —
    /// the test for "this token starts a line", which decides a comment's
    /// own-line/trailing role and a multi-line string's body indentation.
    pub(crate) fn at_line_start(&self) -> bool {
        self.line.is_empty()
    }

    /// Open the current line at `depth`. Called by — and only by — the token
    /// that opens a line, which every caller decides with
    /// [`Printer::at_line_start`]; a line's depth is its first token's.
    pub(crate) fn open_line(&mut self, depth: usize) {
        self.line_depth = depth;
    }

    /// The depth the current line is being written at.
    pub(crate) fn line_depth(&self) -> usize {
        self.line_depth
    }

    /// Append text to the current line. Multi-line values arrive here whole:
    /// only the first line of the run takes the generated indentation, which
    /// is exactly right because the renderer already indents its own
    /// continuation lines.
    pub(crate) fn push(&mut self, text: &str) {
        self.line.push_str(text);
    }

    /// End the current line. An empty line is a blank the author wrote; a
    /// non-empty one is written out.
    pub(crate) fn end_line(&mut self) {
        if self.line.is_empty() {
            self.pending_blanks += 1;
        } else {
            self.flush();
        }
    }

    /// Write the current line, preceded by the blank run the policy allows.
    fn flush(&mut self) {
        let mut blanks = self.pending_blanks.min(Blanks::cap(self.line_depth));
        match self.flushed_depth {
            // Nothing written yet: a file never opens on a blank line.
            None => blanks = 0,
            // A deeper line than the last one is the FIRST line of a body,
            // and a blank there separates a header from its own contents —
            // nothing from nothing. gofmt drops the blank after `{` for the
            // same reason. A blank on the way OUT of a body is kept: it
            // separates the block from what follows, which is a grouping the
            // author meant.
            Some(prev) if self.line_depth > prev => blanks = 0,
            Some(_) => {}
        }
        for _ in 0..blanks {
            self.out.push('\n');
        }
        for _ in 0..self.line_depth {
            self.out.push_str(INDENT_UNIT);
        }
        self.out.push_str(&self.line);
        self.out.push('\n');
        self.flushed_depth = Some(self.line_depth);
        self.line.clear();
        self.pending_blanks = 0;
    }

    /// Finish: flush any unterminated last line and drop the trailing blank
    /// run (a file ends with exactly one line terminator). `terminator` is
    /// the file's own — a CRLF file stays a CRLF file, as `nml fix`'s
    /// insertions already do.
    pub(crate) fn finish(mut self, terminator: &str) -> String {
        if !self.line.is_empty() {
            self.flush();
        }
        if terminator == "\n" {
            self.out
        } else {
            self.out.replace('\n', terminator)
        }
    }
}
