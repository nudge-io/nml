//! Resilient parser for the full NML grammar (RFC 0004 P2).
//!
//! Architecture follows rust-analyzer (RFC 0004 §4.3): the recursive descent runs
//! over a **trivia-stripped** token view and emits a flat [`Event`] list via
//! **markers** (`start`/`complete`/`abandon`); a separate [`build_tree`] step
//! merges those events with the full token stream, **re-attaching trivia**, into
//! the `rowan` green tree. The parser never sees or places trivia, and markers
//! let a node be wrapped *after* its children are parsed (e.g. a fallback chain)
//! without look-ahead.
//!
//! The parser is **purely syntactic**: it recognizes structure only. Semantic
//! interpretation — money vs number, `true`/`false`, template strings, secret
//! namespaces, type validity — is the value layer's job (P3/P5).
//!
//! It is *resilient*: an unexpected token is wrapped in an `Error` node and the
//! parser resynchronizes, collecting every error in one pass. Recursion is
//! depth-bounded and every loop makes forward progress, so any input terminates
//! in linear time (RFC 0004 §9).

use rowan::GreenNode;

use crate::cst::lexer::LexToken;
use crate::cst::syntax::{raw, SyntaxKind};
use crate::error::NmlError;
use crate::span::Span;

/// A flat tree-construction instruction. `Tombstone` is an abandoned marker the
/// builder skips; `Token` consumes the next non-trivia token.
pub(super) enum Event {
    Tombstone,
    Start(SyntaxKind),
    Finish,
    Token,
}

/// Maximum block/value nesting depth — bounds recursion so adversarial nesting
/// cannot overflow the stack (RFC 0004 §9). Matches the legacy `MAX_NESTING_DEPTH`.
const MAX_DEPTH: u32 = 64;

/// A non-trivia token in the parser's view: kind, source text (for contextual
/// keywords and currency codes), start offset (for diagnostics), and whether a
/// `Newline` trivia precedes it (for line-significant decisions — NML keeps
/// `Newline` as lossless trivia, but a fallback `|` must not cross a line).
struct Tok<'a> {
    kind: SyntaxKind,
    text: &'a str,
    offset: usize,
    newline_before: bool,
}

pub(super) struct Parser<'a> {
    toks: Vec<Tok<'a>>,
    pos: usize,
    depth: u32,
    events: Vec<Event>,
    errors: Vec<NmlError>,
}

/// An open node. Completed into a real node or abandoned (its tombstone is then
/// skipped by the builder). Consumed on use, so every marker is resolved.
#[must_use]
struct Marker {
    pos: usize,
}

impl Marker {
    fn complete(self, p: &mut Parser<'_>, kind: SyntaxKind) {
        p.events[self.pos] = Event::Start(kind);
        p.events.push(Event::Finish);
    }

    fn abandon(self, p: &mut Parser<'_>) {
        // Drop a still-empty tombstone at the tail; a tombstone with events after
        // it is left in place and skipped by the builder.
        if self.pos == p.events.len() - 1 {
            debug_assert!(matches!(p.events[self.pos], Event::Tombstone));
            p.events.pop();
        }
    }
}

impl<'a> Parser<'a> {
    pub(super) fn new(tokens: &[LexToken<'a>]) -> Self {
        let mut toks = Vec::new();
        let mut newline_before = false;
        for t in tokens {
            if t.kind.is_trivia() {
                newline_before |= t.kind == SyntaxKind::Newline;
                continue;
            }
            toks.push(Tok {
                kind: t.kind,
                text: t.text,
                offset: t.offset,
                newline_before,
            });
            newline_before = false;
        }
        Self {
            toks,
            pos: 0,
            depth: 0,
            events: Vec::new(),
            errors: Vec::new(),
        }
    }

    // ── token cursor ──────────────────────────────────────────────────────
    fn current(&self) -> SyntaxKind {
        self.nth(0)
    }

    fn nth(&self, n: usize) -> SyntaxKind {
        self.toks.get(self.pos + n).map_or(SyntaxKind::Eof, |t| t.kind)
    }

    fn current_text(&self) -> &'a str {
        self.toks.get(self.pos).map_or("", |t| t.text)
    }

    fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    /// At a contextual keyword (`is`/`by`/`as`) **on the current line**. These
    /// keywords continue a declaration header, so — like every other
    /// line-significant decision ([`Self::at_fallback_pipe`],
    /// [`Self::at_field_type`], [`Self::at_currency`]) — they must not be picked
    /// up from the start of the next entry. (Legacy is protected by its `Newline`
    /// token; NML keeps `Newline` as trivia, so the rule is explicit here.)
    fn at_kw(&self, kw: &str) -> bool {
        self.at(SyntaxKind::Ident) && !self.newline_before() && self.current_text() == kw
    }

    /// Whether a `Newline` separates the current token from the previous one.
    fn newline_before(&self) -> bool {
        self.toks.get(self.pos).is_some_and(|t| t.newline_before)
    }

    fn at_eof(&self) -> bool {
        self.current() == SyntaxKind::Eof
    }

    // ── event emission ────────────────────────────────────────────────────
    fn start(&mut self) -> Marker {
        let pos = self.events.len();
        self.events.push(Event::Tombstone);
        Marker { pos }
    }

    /// Consume the current token into the tree (never `Eof`).
    fn bump(&mut self) {
        if !self.at_eof() {
            self.events.push(Event::Token);
            self.pos += 1;
        }
    }

    fn bump_as(&mut self, kind: SyntaxKind) {
        let m = self.start();
        self.bump();
        m.complete(self, kind);
    }

    fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: SyntaxKind, what: &str) {
        if !self.eat(kind) {
            self.error(&format!("expected {what}"));
        }
    }

    fn error(&mut self, message: &str) {
        if self.errors.len() < super::MAX_ERRORS {
            let offset = self
                .toks
                .get(self.pos)
                .or_else(|| self.toks.last())
                .map_or(0, |t| t.offset);
            self.errors
                .push(NmlError::parse(message.to_string(), Span::empty(offset)));
        }
    }

    /// Panic-mode recovery: wrap the current token in an `Error` node. Consumes
    /// one token ⇒ forward progress.
    fn err_recover(&mut self, message: &str) {
        self.error(message);
        if !self.at_eof() {
            self.bump_as(SyntaxKind::Error);
        }
    }

    // ── grammar ───────────────────────────────────────────────────────────
    /// File body. The `Root` node is opened by [`build_tree`] so it can own
    /// leading/trailing trivia losslessly.
    pub(super) fn parse_root(&mut self) {
        while !self.at_eof() {
            self.declaration();
        }
    }

    fn declaration(&mut self) {
        if self.at(SyntaxKind::LBracket) {
            self.array_decl();
        } else if self.at(SyntaxKind::Ident) {
            match self.current_text() {
                "const" => self.const_decl(),
                "template" => self.template_decl(),
                "oneof" => self.oneof_decl(),
                _ => self.block_decl(),
            }
        } else {
            self.err_recover("expected a declaration");
        }
    }

    /// `keyword name (is parent, …)? : body?`
    fn block_decl(&mut self) {
        let m = self.start();
        self.bump(); // keyword
        self.name();
        self.extends_clause();
        if self.eat(SyntaxKind::Colon) {
            self.body();
        }
        m.complete(self, SyntaxKind::BlockDecl);
    }

    /// `[] item_keyword name : body?`
    fn array_decl(&mut self) {
        let m = self.start();
        self.bump(); // [
        self.expect(SyntaxKind::RBracket, "']'");
        self.expect(SyntaxKind::Ident, "an item keyword"); // item keyword
        self.name();
        if self.eat(SyntaxKind::Colon) {
            self.body();
        }
        m.complete(self, SyntaxKind::ArrayDecl);
    }

    /// `const name = value`
    fn const_decl(&mut self) {
        let m = self.start();
        self.bump(); // const
        self.name();
        self.expect(SyntaxKind::Eq, "'='");
        self.value_block();
        m.complete(self, SyntaxKind::ConstDecl);
    }

    /// `template name : value`
    fn template_decl(&mut self) {
        let m = self.start();
        self.bump(); // template
        self.name();
        self.expect(SyntaxKind::Colon, "':'");
        self.value_block();
        m.complete(self, SyntaxKind::TemplateDecl);
    }

    /// `oneof name by disc (as enum)? (= "default")? : arm+`
    fn oneof_decl(&mut self) {
        let m = self.start();
        self.bump(); // oneof
        self.name();
        if self.at_kw("by") {
            self.bump();
            self.expect(SyntaxKind::Ident, "a discriminator");
        } else {
            self.error("expected 'by <discriminator>'");
        }
        if self.at_kw("as") {
            self.bump();
            self.expect(SyntaxKind::Ident, "an enum name");
        }
        if self.eat(SyntaxKind::Eq) {
            self.expect(SyntaxKind::String, "a quoted default discriminator value");
        }
        self.expect(SyntaxKind::Colon, "':'");
        if self.eat(SyntaxKind::Indent) {
            self.repeat_until_dedent(Self::oneof_arm);
        }
        m.complete(self, SyntaxKind::OneOfDecl);
    }

    /// The arm arrow, with migration guidance (RFC 0006): accepts `->`; a
    /// stale `=>` gets the one-character fix named and is wrapped in an
    /// `Error` node like every other recovery, so the whole file's stale
    /// arrows surface in one parse. Every arm-shaped production calls this
    /// — the guidance is owned once, not re-remembered per production.
    fn expect_arrow(&mut self) {
        if self.at(SyntaxKind::FatArrow) {
            self.err_recover("'=>' was replaced by '->' (RFC 0006)");
        } else {
            self.expect(SyntaxKind::Arrow, "'->'");
        }
    }

    /// `"value" -> Model`
    fn oneof_arm(&mut self) {
        let m = self.start();
        self.expect(SyntaxKind::String, "a quoted discriminator value");
        self.expect_arrow();
        self.expect(SyntaxKind::Ident, "a variant model name");
        m.complete(self, SyntaxKind::OneOfArm);
    }

    /// `is Parent (, Parent)*`
    fn extends_clause(&mut self) {
        if !self.at_kw("is") {
            return;
        }
        let m = self.start();
        self.bump(); // is
        self.expect(SyntaxKind::Ident, "a parent name");
        while self.eat(SyntaxKind::Comma) {
            self.expect(SyntaxKind::Ident, "a parent name");
        }
        m.complete(self, SyntaxKind::Extends);
    }

    /// The declaration/property name, wrapped for typed access.
    fn name(&mut self) {
        if self.at(SyntaxKind::Ident) {
            self.bump_as(SyntaxKind::Name);
        } else {
            self.error("expected a name");
        }
    }

    /// `INDENT entry* DEDENT`, depth-bounded (over-deep tail consumed iteratively).
    fn body(&mut self) {
        if !self.at(SyntaxKind::Indent) {
            return;
        }
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.error("maximum nesting depth exceeded");
            self.skip_block();
            self.depth -= 1;
            return;
        }
        let m = self.start();
        self.bump(); // Indent
        self.repeat_until_dedent(Self::entry);
        m.complete(self, SyntaxKind::Body);
        self.depth -= 1;
    }

    fn entry(&mut self) {
        match self.current() {
            SyntaxKind::Pipe => self.modifier(),
            SyntaxKind::Dot => self.shared_property(),
            SyntaxKind::Dash => self.list_item(),
            // A routing arm (house idiom): `@selector -> Target` or
            // `else -> Target`. The selector — a `Role` token or the `else`
            // contextual keyword — is an arm ONLY when immediately followed by
            // the arrow (the token stream is trivia-free, so `nth(1)` is the next
            // real token). This keeps `else` usable as a property name AND lets a
            // stray `@…` with no arrow fall through to graceful error recovery
            // instead of an over-eager arm parse. A `FatArrow` still routes here
            // so `@x => y` gets the "'=>' was replaced by '->'" guidance.
            SyntaxKind::Role
                if matches!(self.nth(1), SyntaxKind::Arrow | SyntaxKind::FatArrow) =>
            {
                self.arm()
            }
            SyntaxKind::Ident
                if self.current_text() == "else"
                    && matches!(self.nth(1), SyntaxKind::Arrow | SyntaxKind::FatArrow) =>
            {
                self.arm()
            }
            SyntaxKind::Ident => self.ident_entry(),
            _ => self.err_recover("unexpected token in block body"),
        }
    }

    /// `(@selector | else) -> Target` — a routing arm (RFC 0006 arrow). The
    /// selector is a `Role` token or the `else` keyword; the RHS is a target
    /// identifier (`-> Name`, a declared-item reference) or a string literal
    /// (`-> "workflows/pro.workflow.nml"` — RFC 0007 §6, for flat routers
    /// whose targets are paths/URLs). The grammar is permissive about *where*
    /// arms appear — the schema restricts them (e.g. RFC 0018's `denial:`
    /// block) and validates the selector and target shapes.
    fn arm(&mut self) {
        let m = self.start();
        // selector: a role token (`@plan/Pro`) or `else` (a plain ident).
        self.bump();
        self.expect_arrow();
        if matches!(self.current(), SyntaxKind::Ident | SyntaxKind::String) {
            self.bump();
        } else {
            self.error("expected an arm target (a name or a string literal)");
        }
        m.complete(self, SyntaxKind::Arm);
    }

    /// `name = value` | `name : body` | `name TypeExpr ? (= value)?`
    fn ident_entry(&mut self) {
        let m = self.start();
        self.bump(); // name
        if self.eat(SyntaxKind::Eq) {
            self.value_block();
            m.complete(self, SyntaxKind::Property);
        } else if self.eat(SyntaxKind::Colon) {
            self.body();
            m.complete(self, SyntaxKind::NestedBlock);
        } else if self.at_field_type() {
            self.type_expr();
            // Suffixes: `?` (optional) and/or `+` (positional shorthand, RFC 0005
            // §16). Canonical order is `?+` — `?` is eaten first — and the
            // extracted flags are independent booleans (order in the AST is free).
            self.eat(SyntaxKind::Question);
            self.eat(SyntaxKind::Plus);
            if self.eat(SyntaxKind::Eq) {
                self.value_block();
            }
            m.complete(self, SyntaxKind::FieldDef);
        } else {
            self.error("expected '=', ':', or a type");
            m.complete(self, SyntaxKind::Error);
        }
    }

    /// `| name (= value | : list | TypeExpr ?)`
    fn modifier(&mut self) {
        let m = self.start();
        self.bump(); // |
        self.expect(SyntaxKind::Ident, "a modifier name");
        if self.eat(SyntaxKind::Eq) {
            self.value_block();
        } else if self.eat(SyntaxKind::Colon) {
            self.body();
        } else if self.at_field_type() {
            self.type_expr();
            self.eat(SyntaxKind::Question);
        } else {
            self.error("expected '=', ':', or a type after a modifier");
        }
        m.complete(self, SyntaxKind::Modifier);
    }

    /// `. name (: body | = value)`
    fn shared_property(&mut self) {
        let m = self.start();
        self.bump(); // .
        self.expect(SyntaxKind::Ident, "a shared-property name");
        if self.eat(SyntaxKind::Colon) {
            self.body();
        } else if self.eat(SyntaxKind::Eq) {
            self.value_block();
        } else {
            self.error("expected ':' or '=' after a shared property");
        }
        m.complete(self, SyntaxKind::SharedProperty);
    }

    /// `- ("string" | @role | Name (: body)?)`
    fn list_item(&mut self) {
        let m = self.start();
        self.bump(); // -
        match self.current() {
            // A scalar key: `- "/api"` or, with a body, `- "/api":` + indented block
            // (scalar-key-with-body — the shorthand fills the model's `!` field, the
            // body fills the rest).
            SyntaxKind::String | SyntaxKind::Number => {
                self.value();
                if self.eat(SyntaxKind::Colon) {
                    self.body();
                }
            }
            SyntaxKind::Role | SyntaxKind::Secret => self.bump(),
            SyntaxKind::Ident => {
                self.bump();
                if self.eat(SyntaxKind::Colon) {
                    self.body();
                }
            }
            _ => self.error("expected a list item after '-'"),
        }
        m.complete(self, SyntaxKind::ListItem);
    }

    // ── type expressions ──────────────────────────────────────────────────
    fn at_type_start(&self) -> bool {
        matches!(
            self.current(),
            SyntaxKind::Ident | SyntaxKind::LBracket | SyntaxKind::LParen
        )
    }

    /// A field/modifier type must follow the name **on the same line**. Without
    /// this, `name\nother` would mis-parse as the field `name: other` (consuming
    /// the next entry as a type) — the same line-significance rule as a fallback
    /// `|` (see [`Self::at_fallback_pipe`]).
    fn at_field_type(&self) -> bool {
        !self.newline_before() && self.at_type_start()
    }

    /// `[]TypeExpr` | `(TypeExpr (| TypeExpr)*)` | `(TypeExpr -> TypeExpr)` | Name
    fn type_expr(&mut self) {
        if self.depth_guarded_type() {
            return;
        }
        let m = self.start();
        if self.eat(SyntaxKind::LBracket) {
            self.expect(SyntaxKind::RBracket, "']'");
            self.type_expr();
        } else if self.eat(SyntaxKind::LParen) {
            self.type_expr();
            // `(K -> V)` — a typed arm set (RFC 0007). The arrow, like the
            // union pipe, is only ever consumed *inside* the parens: a bare
            // `K -> V` at type position is a parse error, which is what keeps
            // the field-suffix `?` unambiguous (it always binds to the field).
            // `expect_arrow` gives a `=>` the RFC 0006 guidance error.
            if matches!(self.current(), SyntaxKind::Arrow | SyntaxKind::FatArrow) {
                self.expect_arrow();
                self.type_expr();
            } else {
                while self.eat(SyntaxKind::Pipe) {
                    self.type_expr();
                }
            }
            self.expect(SyntaxKind::RParen, "')'");
        } else {
            self.expect(SyntaxKind::Ident, "a type name");
        }
        m.complete(self, SyntaxKind::TypeExpr);
        self.depth -= 1;
    }

    /// Enter the value/type recursion guard; on overflow, emit a diagnostic and
    /// consume one token so the caller still progresses. Returns `true` if the
    /// limit was hit (caller must return without recursing).
    fn depth_guarded_type(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.error("maximum type nesting depth exceeded");
            self.depth -= 1;
            if !self.at_eof() {
                self.bump_as(SyntaxKind::Error);
            }
            return true;
        }
        false
    }

    // ── values ────────────────────────────────────────────────────────────
    /// A value with an optional leading INDENT/trailing DEDENT (block form).
    fn value_block(&mut self) {
        let indented = self.eat(SyntaxKind::Indent);
        self.value_or_fallback();
        if indented {
            self.eat(SyntaxKind::Dedent);
        }
    }

    /// `value (| value)*` — wrapped in a `Fallback` node only if a same-line `|`
    /// follows. A `|` at the start of the next line is a modifier, not a fallback
    /// continuation, so the chain must not cross a newline.
    fn value_or_fallback(&mut self) {
        let m = self.start();
        self.value();
        if self.at_fallback_pipe() {
            while self.at_fallback_pipe() {
                self.bump(); // |
                self.value();
            }
            m.complete(self, SyntaxKind::Fallback);
        } else {
            m.abandon(self);
        }
    }

    fn at_fallback_pipe(&self) -> bool {
        self.at(SyntaxKind::Pipe) && !self.newline_before()
    }

    fn value(&mut self) {
        if self.at(SyntaxKind::LBracket) {
            self.array_value();
            return;
        }
        let m = self.start();
        match self.current() {
            SyntaxKind::String | SyntaxKind::Role | SyntaxKind::Secret | SyntaxKind::Ident => {
                self.bump()
            }
            SyntaxKind::Number => self.number_value(),
            SyntaxKind::Dash => {
                self.bump();
                if self.at(SyntaxKind::Number) {
                    self.number_value();
                } else {
                    self.error("expected a number after '-'");
                }
            }
            _ => self.error("expected a value"),
        }
        m.complete(self, SyntaxKind::Value);
    }

    /// A number, optionally followed by a **same-line** 3-letter currency code
    /// (money). The currency is recognized syntactically; validity is the value
    /// layer's job.
    fn number_value(&mut self) {
        self.bump(); // number
        if self.at_currency() {
            self.bump();
        }
    }

    /// A currency code must follow the number on the same line — otherwise a
    /// 3-uppercase identifier starting the *next* entry (e.g. a `USD = …`
    /// property) would be swallowed. Same line-significance rule as
    /// [`Self::at_fallback_pipe`] / [`Self::at_field_type`].
    fn at_currency(&self) -> bool {
        self.at(SyntaxKind::Ident) && !self.newline_before() && is_currency(self.current_text())
    }

    /// `[ value (, value)* ,? ]`
    fn array_value(&mut self) {
        if self.depth_guarded_type() {
            return;
        }
        let m = self.start();
        self.bump(); // [
        loop {
            match self.current() {
                SyntaxKind::RBracket => {
                    self.bump();
                    break;
                }
                SyntaxKind::Eof => break,
                _ => {
                    let before = self.pos;
                    self.value();
                    if !self.eat(SyntaxKind::Comma) && !self.at(SyntaxKind::RBracket) {
                        self.error("expected ',' or ']' in array literal");
                    }
                    if self.pos == before {
                        // value() did not advance (unexpected token): recover so
                        // the loop cannot spin.
                        self.err_recover("unexpected token in array literal");
                    }
                }
            }
        }
        m.complete(self, SyntaxKind::ArrayValue);
        self.depth -= 1;
    }

    // ── shared loop / recovery helpers ────────────────────────────────────
    /// Repeat `f` until a `Dedent` (consumed) or `Eof`, guaranteeing progress.
    fn repeat_until_dedent(&mut self, f: fn(&mut Self)) {
        loop {
            match self.current() {
                SyntaxKind::Dedent => {
                    self.bump();
                    break;
                }
                SyntaxKind::Eof => break,
                _ => {
                    let before = self.pos;
                    f(self);
                    if self.pos == before {
                        self.err_recover("unexpected token");
                    }
                }
            }
        }
    }

    /// Consume a balanced `Indent … Dedent` run as one `Error` node, iteratively
    /// (the depth-limit escape hatch).
    fn skip_block(&mut self) {
        let m = self.start();
        let mut nesting = 0u32;
        loop {
            match self.current() {
                SyntaxKind::Indent => {
                    self.bump();
                    nesting += 1;
                }
                SyntaxKind::Dedent => {
                    self.bump();
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                }
                SyntaxKind::Eof => break,
                _ => self.bump(),
            }
        }
        m.complete(self, SyntaxKind::Error);
    }

    pub(super) fn finish_parse(self) -> (Vec<Event>, Vec<NmlError>) {
        (self.events, self.errors)
    }
}

/// A 3-uppercase-letter currency code (e.g. `USD`), recognized syntactically.
fn is_currency(text: &str) -> bool {
    text.len() == 3 && text.bytes().all(|b| b.is_ascii_uppercase())
}

/// Merge the event list with the full (trivia-bearing) token stream into the
/// green tree, applying the RFC 0004 §4.3 **comment-attachment policy**: an
/// own-line comment attaches as *leading* trivia of the following node; a
/// same-line *trailing* comment attaches to the preceding node. Whitespace and
/// newlines are invisible to consumers, so only comment placement is meaningful;
/// the total token sequence is unchanged, so the tree stays byte-faithful.
/// `Tombstone` events (abandoned markers) are skipped.
pub(super) fn build_tree(full: &[LexToken<'_>], events: &[Event]) -> GreenNode {
    let mut b = TreeBuilder {
        inner: rowan::GreenNodeBuilder::new(),
        full,
        cursor: 0,
        at_line_start: true,
        indent_stack: vec![0],
        deferred: Vec::new(),
    };
    b.inner.start_node(raw(SyntaxKind::Root));
    for event in events {
        match event {
            Event::Tombstone => {}
            // A same-line trailing comment belongs to the node ending here.
            Event::Finish => {
                b.attach_trailing();
                b.inner.finish_node();
            }
            // …or to the parent, when a sibling node starts after it.
            Event::Start(kind) => {
                b.attach_trailing();
                b.inner.start_node(raw(*kind));
            }
            Event::Token => {
                b.flush_leading();
                if b.cursor < full.len() {
                    b.bump();
                }
            }
        }
    }
    // At EOF every scope has closed, so any still-deferred comment belongs to the
    // root. Collapsing the indent stack first makes the release unconditional, so a
    // deferred comment can never be stranded (losslessness holds even if error
    // recovery left the stack unbalanced). Then flush leftover trailing trivia.
    b.indent_stack.truncate(1);
    b.release_deferred();
    while b.cursor < full.len() {
        b.bump();
    }
    b.inner.finish_node(); // Root
    b.inner.finish()
}

/// Green-tree builder applying the RFC 0004 §4.3 comment-attachment policy.
///
/// Tracks the offside indent stack so an own-line comment that a body-closing
/// `Dedent` separates from the following node is **deferred** past the (zero-width)
/// dedent into the scope its column belongs to, rather than trapped in the closing
/// body. Reordering only zero-width dedents ahead of the held comment keeps the
/// emitted token text byte-identical to the source.
struct TreeBuilder<'a> {
    inner: rowan::GreenNodeBuilder<'static>,
    full: &'a [LexToken<'a>],
    cursor: usize,
    /// `true` while only whitespace/layout has been emitted since the last
    /// newline — i.e. the next comment would be *own-line*, not trailing.
    at_line_start: bool,
    /// Offside body-indent widths of the open scopes (mirrors the lexer's stack);
    /// `indent_stack.last()` is the indentation of the innermost open block.
    indent_stack: Vec<usize>,
    /// Own-line comments held back from a closing body, each with its source
    /// column and the token indices (comment + trailing layout) to emit, in
    /// source order. Released into a scope once that scope is no deeper than the
    /// comment's column.
    deferred: Vec<DeferredComment>,
}

/// A comment held back from a closing body until its target (outer) scope opens.
struct DeferredComment {
    column: usize,
    tokens: Vec<usize>,
}

impl TreeBuilder<'_> {
    /// Emit one token by index, updating line-start tracking and the indent stack
    /// (the latter only for the zero-width layout markers).
    fn emit(&mut self, idx: usize) {
        let tok = &self.full[idx];
        self.inner.token(raw(tok.kind), tok.text);
        self.at_line_start = match tok.kind {
            SyntaxKind::Newline => true,
            SyntaxKind::Whitespace | SyntaxKind::Indent | SyntaxKind::Dedent => self.at_line_start,
            _ => false, // content (including a comment) ends "line start"
        };
        match tok.kind {
            // An `Indent`'s width is carried by the leading whitespace the lexer
            // emits immediately after it (zero if the line is unindented).
            SyntaxKind::Indent => {
                let width = match self.full.get(idx + 1) {
                    Some(ws) if ws.kind == SyntaxKind::Whitespace => ws.text.len(),
                    _ => 0,
                };
                self.indent_stack.push(width);
            }
            SyntaxKind::Dedent if self.indent_stack.len() > 1 => {
                self.indent_stack.pop();
            }
            _ => {}
        }
    }

    fn bump(&mut self) {
        self.emit(self.cursor);
        self.cursor += 1;
    }

    /// Flush leading trivia into the currently-open node. Releases any deferred
    /// comment that the just-opened scope now owns, then walks the trivia run,
    /// holding back own-line comments that a closing dedent places in an outer
    /// scope (RFC 0004 §4.3).
    fn flush_leading(&mut self) {
        self.release_deferred();
        while self.cursor < self.full.len() && self.full[self.cursor].kind.is_trivia() {
            if self.full[self.cursor].kind == SyntaxKind::Comment && self.should_defer() {
                self.defer_comment();
            } else {
                self.bump();
            }
        }
    }

    /// Emit deferred comments whose target scope is now open — i.e. whose column
    /// is no shallower than the innermost open block's indentation. Released
    /// front-first (source order) and stopping at the first not-yet-in-scope group,
    /// so emission is always byte-faithful. (For the pathological case of comments
    /// indented *non-monotonically* within one dedent gap, this can attach a
    /// comment to an outer scope rather than its exact column — a deliberate trade
    /// of attachment precision for the foundational losslessness invariant, since
    /// re-ordering the held groups would move non-zero-width tokens.)
    fn release_deferred(&mut self) {
        let top = *self.indent_stack.last().expect("indent stack is never empty");
        while self.deferred.first().is_some_and(|d| d.column >= top) {
            let d = self.deferred.remove(0);
            for idx in d.tokens {
                self.emit(idx);
            }
        }
    }

    /// Whether the comment at the cursor is own-line, sits before a body-closing
    /// dedent, and is indented shallower than the scope that dedent closes — in
    /// which case it belongs to an outer scope and must be deferred past the dedent.
    fn should_defer(&self) -> bool {
        let col = self.column_at(self.cursor);
        self.is_own_line(self.cursor)
            && self.dedent_follows(self.cursor)
            && col < *self.indent_stack.last().expect("indent stack is never empty")
    }

    /// Hold the cursor comment and its trailing layout (up to the next comment or
    /// non-trivia) as one deferred group, advancing past them without emitting.
    fn defer_comment(&mut self) {
        let column = self.column_at(self.cursor);
        let mut tokens = vec![self.cursor];
        self.cursor += 1;
        while self.cursor < self.full.len()
            && matches!(
                self.full[self.cursor].kind,
                SyntaxKind::Whitespace | SyntaxKind::Newline
            )
        {
            tokens.push(self.cursor);
            self.cursor += 1;
        }
        self.deferred.push(DeferredComment { column, tokens });
    }

    /// Source column of the token at `idx`: the width of its line's leading
    /// indentation (the whitespace immediately preceding it), or zero at column 0.
    fn column_at(&self, idx: usize) -> usize {
        match idx.checked_sub(1).map(|p| &self.full[p]) {
            Some(ws) if ws.kind == SyntaxKind::Whitespace => ws.text.len(),
            _ => 0,
        }
    }

    /// Whether only whitespace precedes the token at `idx` on its line.
    fn is_own_line(&self, idx: usize) -> bool {
        for tok in self.full[..idx].iter().rev() {
            match tok.kind {
                SyntaxKind::Newline => return true,
                SyntaxKind::Whitespace => continue,
                _ => return false, // code precedes → trailing comment
            }
        }
        true // start of file
    }

    /// Whether the next non-trivia token after the comment at `idx` is a `Dedent`
    /// (so the comment is the last thing in a body about to close).
    fn dedent_follows(&self, idx: usize) -> bool {
        self.full[idx + 1..]
            .iter()
            .find(|t| !t.kind.is_trivia())
            .is_some_and(|t| t.kind == SyntaxKind::Dedent)
    }

    /// Attach a same-line trailing comment to the currently-open node. A no-op at
    /// the start of a line (a comment there is own-line — left for the following
    /// node's leading trivia).
    fn attach_trailing(&mut self) {
        if self.at_line_start {
            return;
        }
        let mut len = 0;
        for (i, tok) in self.full[self.cursor..].iter().enumerate() {
            match tok.kind {
                SyntaxKind::Comment => {
                    len = i + 1;
                    break;
                }
                SyntaxKind::Whitespace => continue,
                _ => break, // newline / content → no same-line comment
            }
        }
        for _ in 0..len {
            self.bump();
        }
    }
}
