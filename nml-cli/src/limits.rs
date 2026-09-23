//! `nml limits` — the toolkit's
//! BOUNDS, published as a first-class surface and kept honest by a census.
//!
//! A configuration tool's limits are part of its contract: an operator
//! sizing a tenant tree, or a schema author wondering why a 70-deep
//! `uses` stack is refused, needs the numbers without reading the
//! source. Comparable tools ship them as prose that rots — `clang`'s
//! `-ftemplate-depth`, `jq`'s recursion bound and `yq`'s document
//! limits are all discovered from an error message. Here every bound
//! has ONE owner — the constant's own doc comment — the table is
//! generated from it, and a test fails the build when the generated
//! table is stale, when a new bound appears unclassified, or when a
//! published bound is named by no test.
//!
//! Every bound is classified on three axes (a two-way "tenant/operator"
//! label misfiled eight bounds and hid six editor bounds a tenant
//! reaches):
//!
//! * `reach` — WHO can reach it. `content`: a file, manifest, glob or
//!   tree a tenant commits (in CI the checked file is the tenant's, so
//!   every bound behind a checked file is content-reachable — the
//!   security-relevant class). `peer`: the process at the other end of
//!   the editor's wire. `operator`: only what the operator installs or
//!   types — a class with NO member in this tree (every walk of the
//!   WORKSPACE root is within a tenant's reach, since a tenant pads it).
//!   `internal`: an in-memory guard invisible to every result —
//!   never published, listed with its reason.
//! * `guards` — WHAT it bounds. `work`: steps or time. `memory`: bytes
//!   held or stack depth (every nesting cap). `output`: what is
//!   reported or echoed. `domain`: a representational ceiling of the
//!   language or the platform.
//! * `surface` — WHERE it lives: `kernel` (nml-core, nml-validate),
//!   `cli` (nml-cli), `editor` (nml-lsp).
//!
//! **Single source.** Each published constant carries, as the last line
//! of its doc comment, `LIMIT: reach=<reach> guards=<guards>
//! surface=<surface> shown="<value as a human reads it>" — <what>`, and
//! each deliberately unpublished one `UNPUBLISHED: <why>`; the table
//! (`limits_table.rs`) is GENERATED from those lines by the census and
//! committed, and a test refuses a stale one — so a bound is classified
//! exactly once, beside the code that enforces it, and the table cannot
//! drift from the declaration by construction.
//!
//! **Pin census.** Every published bound is NAMED by at least one test
//! (an identifier use in a `tests/` file or a `#[cfg(test)]` module,
//! comments and string literals excluded), so its value is pinned by
//! behaviour, not only by this table; a domain ceiling that has no
//! behaviour to pin is waived here WITH A REASON. The census is by bare
//! name, so a name two bounds share (`MAX_DEPTH`, `MAX_ECHO`) must be
//! named by a test IN ITS OWN FILE or in the
//! `tests/` subtree beside it — a mention elsewhere could be the other
//! bound's.
//!
//! **Recognizer contract.** The census finds a bound by its DECLARATION
//! and its NAME, and nothing else: at a line start (any indentation),
//! `[pub] const NAME: T = RHS;`, `[pub] static NAME: T = RHS;`, or
//! `[pub] const fn name(`; a name is bound-shaped when one of its
//! `_`-separated words is `MAX`, `MIN`, `CAP`, `LIMIT`, `BUDGET`,
//! `BOUND`, `CEILING`, `FLOOR`, `QUOTA` or `THRESHOLD` (case-insensitive
//! for a `const fn`). A bound spelled any other way — a `let`, a literal
//! in an expression, a name outside that vocabulary — is invisible to
//! it; the naming rule IS the contract, and a reviewer adding a bound
//! spells it into the vocabulary or explains why not.

/// Who can reach a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Content a tenant commits.
    Content,
    /// The process at the other end of the editor's wire.
    Peer,
    /// An in-memory guard invisible to every result (unpublished rows).
    Internal,
}

impl Reach {
    pub fn label(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Peer => "peer",
            Self::Internal => "internal",
        }
    }
}

/// What a bound guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guards {
    Work,
    Memory,
    Output,
    Domain,
}

impl Guards {
    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Memory => "memory",
            Self::Output => "output",
            Self::Domain => "domain",
        }
    }
}

/// Where a bound lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Kernel,
    Cli,
    Editor,
}

impl Surface {
    pub fn label(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Cli => "cli",
            Self::Editor => "editor",
        }
    }
}

/// One published bound.
pub struct Limit {
    /// The declaring file, workspace-relative — the census key.
    pub path: &'static str,
    pub name: &'static str,
    /// The declaration's right-hand side, VERBATIM. The census compares
    /// it to the source, so a value can never change under the table.
    pub expr: &'static str,
    /// The value as a human reads it.
    pub shown: &'static str,
    pub reach: Reach,
    pub guards: Guards,
    pub surface: Surface,
    /// What the bound bounds — the text after the dash of the
    /// constant's `LIMIT:` line, verbatim.
    pub what: &'static str,
}

impl Limit {
    /// The bound's QUALIFIED name — `nml-core::cst::parser::MAX_DEPTH`.
    /// Two different modules declare a `MAX_DEPTH` and two declare a
    /// `MAX_ECHO`; a table that prints the bare name reads as a
    /// contradiction, which is precisely how a hand-kept list of these
    /// goes wrong.
    pub fn qualified(&self) -> String {
        format!("{}::{}", module_of(self.path), self.name)
    }
}

/// `crates/nml-core/src/cst/parser.rs` -> `nml-core::cst::parser`.
pub fn module_of(path: &str) -> String {
    let path = path.strip_suffix(".rs").unwrap_or(path);
    let path = path.strip_prefix("crates/").unwrap_or(path);
    let mut parts: Vec<&str> = path.split('/').collect();
    if let Some(i) = parts.iter().position(|p| *p == "src") {
        parts.remove(i);
    }
    if matches!(parts.last(), Some(&"mod") | Some(&"lib")) {
        parts.pop();
    }
    parts.join("::")
}

/// A bound deliberately NOT published, and why.
pub struct Unpublished {
    pub path: &'static str,
    pub name: &'static str,
    pub why: &'static str,
}

/// A published bound the pin census waives, and why: a domain ceiling
/// with no behaviour a test could reach.
#[cfg(test)]
pub struct Waiver {
    pub path: &'static str,
    pub name: &'static str,
    pub why: &'static str,
}

// The published table (`LIMITS`) and the unpublished list
// (`UNPUBLISHED`), GENERATED from the `LIMIT:` / `UNPUBLISHED:` doc lines
// of every bound-shaped declaration in the tree by the census
// (`limits_table.rs`; `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli --
// limits::tests::the_generated_table_is_fresh` rewrites it, and that test
// refuses a stale one). Content-reachable bounds first, then the peer's;
// by crate (kernel, cli, editor), file and declaration order.
include!("limits_table.rs");

/// Published bounds no test names, each with its reason — domain
/// ceilings only: the value IS the representation, and there is no
/// behaviour at the bound for a test to reach.
#[cfg(test)]
pub const WAIVERS: &[Waiver] = &[
    Waiver {
        path: "crates/nml-core/src/duration/mod.rs",
        name: "MAX_SEGMENTS",
        why: "the segment capacity IS the unit count, asserted at compile time (`const _: () = assert!(MAX_SEGMENTS == DurationUnit::ALL.len())`)",
    },
    Waiver {
        path: "crates/nml-core/src/duration/mod.rs",
        name: "STD_MAX_NANOS",
        why: "`std::time::Duration::MAX` in nanoseconds — the value-domain ceiling every duration overflow test reaches through the type's own arithmetic",
    },
];

/// The table's legend — the one place the reach/guards/surface vocabulary
/// is spelled for the reader.
///
/// It names the classes the table PRINTS (`content`, `peer`) and the one
/// the closing line counts (`internal`). `operator` — "only what the
/// operator installs or types" — is a real class of the scheme with no
/// member in this tree, so it is stated in the guide's prose rather than
/// offered here as a group the reader will look for and never find.
pub(crate) fn legend() -> &'static str {
    "LIMITS  (reach: who can reach the bound — content a tenant commits, or the editor's \
     peer; a guard no result can reach is internal and unpublished, counted on the closing \
     line; guards: work, memory, output or a domain ceiling; surface: kernel, cli or editor)"
}

/// What `nml limits` accepts, and its page (`crate::Spec`).
pub(crate) const SPEC: crate::Spec = crate::Spec {
    verb: "limits",
    summary: "Print the toolkit's published bounds: what each bounds, its value, who can \
              reach it (content a tenant commits, or the editor's peer — no published bound \
              is operator-only today), what it guards (work, memory, output, a domain \
              ceiling) and where it lives (kernel, cli, editor). A guard no result can \
              reach is internal and unpublished; the closing line counts them.",
    root: false,
    schema: false,
    strict: false,
    edit: None,
    max_findings: false,
    list: false,
    targets: "",
    arity: crate::Arity::None,
    exits: &[("0", "the table printed"), ("2", "a usage error")],
    examples: &[
        "nml limits",
        "nml limits --json | jq -r 'select(.reach==\"content\") | \"\\(.name)\\t\\(.value)\"'",
    ],
};

/// `nml limits` — the published bounds, aligned, content-reachable
/// first; `--json` emits one `limit` row per bound and the run's
/// closing `summary` row.
pub fn cmd_limits(args: &[String]) -> Result<(), String> {
    crate::parse_invocation(args, &SPEC)?;
    if crate::out::json() {
        for limit in LIMITS {
            crate::out::emit(&serde_json::json!({
                "type": "limit",
                "name": limit.qualified(),
                "value": limit.shown,
                "reach": limit.reach.label(),
                "guards": limit.guards.label(),
                "surface": limit.surface.label(),
                "what": limit.what,
                "declaredIn": limit.path,
                "declaration": limit.expr,
                "published": true,
            }));
        }
        // The classification is DATA, not a secret kept in a test: the
        // bounds this table deliberately does not publish are rows too,
        // each carrying the reason. A reader can then see that the
        // census covered them, rather than wondering what it missed.
        for entry in UNPUBLISHED {
            crate::out::emit(&serde_json::json!({
                "type": "limit",
                "name": format!("{}::{}", module_of(entry.path), entry.name),
                "reach": Reach::Internal.label(),
                "what": entry.why,
                "declaredIn": entry.path,
                "published": false,
            }));
        }
        return Ok(());
    }
    let mut out = format!("{}\n\n", legend());
    let width = LIMITS
        .iter()
        .map(|l| l.qualified().len())
        .max()
        .unwrap_or(0);
    let mut reach = None;
    for limit in LIMITS {
        if reach != Some(limit.reach) {
            reach = Some(limit.reach);
            out.push_str(&format!("[{}]\n", limit.reach.label()));
        }
        out.push_str(&format!(
            "  {:<width$}  {:>16}  {:<6}  {:<6}  {}\n",
            limit.qualified(),
            limit.shown,
            limit.guards.label(),
            limit.surface.label(),
            limit.what
        ));
    }
    out.push_str(&format!(
        "\n{} internal bound(s) are deliberately unpublished (invisible to every result); \
         `nml limits --json` lists each with its reason.\n",
        UNPUBLISHED.len()
    ));
    crate::out::say_str(&out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nml_validate::test_support::scan::{
        blank_comments_and_strings, cfg_test_blocks, sources, sources_all, workspace,
    };
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Every `[pub[(..)]] const NAME: TY = RHS;`, `[pub[(..)]] static
    /// NAME: TY = RHS;` and `[pub[(..)]] const fn name(` in a source
    /// (the recognizer contract in the module doc), with the right-hand
    /// side normalized to single spaces. A declaration is recognized at
    /// a LINE START, so a mention in prose or in a string is never a
    /// hit; its VALUE may run over as many lines as it likes — a
    /// scanner that read one line would silently miss exactly the bounds
    /// a reviewer most wants listed. A `const fn`'s "value" is its
    /// signature up to the body.
    fn declarations(text: &str) -> Vec<(&str, String)> {
        let mut out = Vec::new();
        let mut here = 0usize;
        for line in text.lines() {
            let start = here;
            here += line.len() + 1;
            let rest = line.trim_start();
            let rest = match rest.strip_prefix("pub") {
                Some(after) => {
                    let after = after.trim_start();
                    match after.strip_prefix('(') {
                        Some(vis) => match vis.split_once(')') {
                            Some((_, tail)) => tail.trim_start(),
                            None => continue,
                        },
                        None => after,
                    }
                }
                None => rest,
            };
            let Some(body) = rest
                .strip_prefix("const ")
                .or_else(|| rest.strip_prefix("static "))
            else {
                continue;
            };
            let from = start + (line.len() - body.len());
            let tail = &text[from..];
            if let Some(sig) = body.strip_prefix("fn ") {
                let Some(open) = sig.find('(') else { continue };
                let name = sig[..open].trim();
                let Some(brace) = tail.find('{') else {
                    continue;
                };
                out.push((
                    name,
                    tail[..brace]
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
                continue;
            }
            // From the name onwards, across lines, up to the `;`.
            let Some(semi) = tail.find(';') else { continue };
            let Some((name, after)) = tail[..semi].split_once(':') else {
                continue;
            };
            let Some((_ty, rhs)) = after.split_once('=') else {
                continue;
            };
            out.push((
                name.trim(),
                rhs.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
        }
        out
    }

    /// A bound-shaped name: one of its `_`-separated words is in the
    /// vocabulary (case-insensitive, so a `const fn input_cap` counts).
    /// Whole words, not substrings: `CAPACITY` and `CAPABILITIES` are
    /// not `CAP`.
    fn bound_shaped(name: &str) -> bool {
        const WORDS: &[&str] = &[
            "MAX",
            "MIN",
            "CAP",
            "LIMIT",
            "BUDGET",
            "BOUND",
            "CEILING",
            "FLOOR",
            "QUOTA",
            "THRESHOLD",
        ];
        name.split('_')
            .any(|word| WORDS.iter().any(|w| word.eq_ignore_ascii_case(w)))
    }

    /// The `///` block directly above the declaration of `name` in
    /// `text` (a `const`, a `static` or a `const fn`) with the
    /// declaration's line number, or `None` when the declaration is
    /// missing or has no doc comment.
    fn doc_block_above(text: &str, name: &str) -> Option<(usize, Vec<String>)> {
        let lines: Vec<&str> = text.lines().collect();
        let decl = lines.iter().position(|l| {
            let rest = l.trim_start();
            let rest = rest.strip_prefix("pub").map_or(rest, |a| {
                let a = a.trim_start();
                a.strip_prefix('(')
                    .and_then(|v| v.split_once(')'))
                    .map_or(a, |(_, t)| t.trim_start())
            });
            ["const ", "static "].iter().any(|kw| {
                rest.strip_prefix(kw).is_some_and(|b| {
                    let b = b.trim_start();
                    b.starts_with(&format!("{name}:"))
                        || b.strip_prefix("fn ")
                            .is_some_and(|f| f.trim_start().starts_with(&format!("{name}(")))
                })
            })
        })?;
        let mut block = Vec::new();
        let mut i = decl;
        while i > 0 && lines[i - 1].trim_start().starts_with("///") {
            i -= 1;
            block.push(
                lines[i]
                    .trim_start()
                    .trim_start_matches("///")
                    .trim()
                    .to_string(),
            );
        }
        block.reverse();
        (!block.is_empty()).then_some((decl, block))
    }

    /// The test regions of the tree: every file under a `tests/`
    /// directory whole, plus every brace-matched `#[cfg(test)] mod`
    /// block of a source file — with comments and string literals
    /// blanked, so only an identifier USE can name a bound.
    fn test_regions() -> Vec<(String, String)> {
        let workspace = workspace();
        let mut regions = Vec::new();
        let mut srcs = Vec::new();
        sources(&workspace.join("crates"), false, &mut srcs);
        sources(&workspace.join("nml-cli").join("src"), false, &mut srcs);
        let mut test_files = Vec::new();
        sources(&workspace.join("crates"), true, &mut test_files);
        sources(&workspace.join("tests"), true, &mut test_files);
        // `tests/` at the workspace root is itself a tests directory.
        let mut root_tests = Vec::new();
        sources_all(&workspace.join("tests"), &mut root_tests);
        test_files.extend(root_tests);
        for file in test_files {
            if let Ok(text) = std::fs::read_to_string(&file) {
                regions.push((
                    file.display().to_string(),
                    blank_comments_and_strings(&text),
                ));
            }
        }
        for file in srcs {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let clean = blank_comments_and_strings(&text);
            for block in cfg_test_blocks(&clean) {
                regions.push((file.display().to_string(), block));
            }
        }
        regions
    }

    fn names_identifier(region: &str, name: &str) -> bool {
        let mut from = 0;
        while let Some(at) = region[from..].find(name) {
            let i = from + at;
            let before = region[..i].chars().next_back();
            let after = region[i + name.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if boundary(before) && boundary(after) {
                return true;
            }
            from = i + name.len();
        }
        false
    }

    /// One parsed `LIMIT:` line — `reach=<r> guards=<g> surface=<s>
    /// shown="<v>" — <what>` — or `None` when the line is not of that
    /// shape (the ratchet then names the file).
    fn parse_limit_line(line: &str) -> Option<(Reach, Guards, Surface, String, String)> {
        let rest = line.strip_prefix("LIMIT: ")?;
        let (head, what) = rest.split_once(" — ")?;
        let mut reach = None;
        let mut guards = None;
        let mut surface = None;
        let mut shown = None;
        let mut rest = head.trim();
        while !rest.is_empty() {
            let (key, after) = rest.split_once('=')?;
            let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
                let (v, tail) = quoted.split_once('"')?;
                (v.to_string(), tail)
            } else {
                match after.split_once(' ') {
                    Some((v, tail)) => (v.to_string(), tail),
                    None => (after.to_string(), ""),
                }
            };
            match key {
                "reach" => {
                    reach = Some(match value.as_str() {
                        "content" => Reach::Content,
                        "peer" => Reach::Peer,
                        _ => return None,
                    })
                }
                "guards" => {
                    guards = Some(match value.as_str() {
                        "work" => Guards::Work,
                        "memory" => Guards::Memory,
                        "output" => Guards::Output,
                        "domain" => Guards::Domain,
                        _ => return None,
                    })
                }
                "surface" => {
                    surface = Some(match value.as_str() {
                        "kernel" => Surface::Kernel,
                        "cli" => Surface::Cli,
                        "editor" => Surface::Editor,
                        _ => return None,
                    })
                }
                "shown" => shown = Some(value),
                _ => return None,
            }
            rest = tail.trim_start();
        }
        Some((reach?, guards?, surface?, shown?, what.to_string()))
    }

    /// Every bound-shaped declaration in the tree with its classification
    /// line — the LAST line of its doc comment — in table order:
    /// content-reachable first, then the peer's; by crate (kernel: core
    /// then validate; cli; editor), file, declaration line.
    fn classified_declarations() -> (Vec<Limit>, Vec<Unpublished>) {
        let workspace = workspace();
        let mut files = Vec::new();
        sources(&workspace.join("crates"), false, &mut files);
        sources(&workspace.join("nml-cli").join("src"), false, &mut files);
        files.sort();
        let mut published: Vec<(usize, usize, Limit)> = Vec::new();
        let mut unpublished: Vec<Unpublished> = Vec::new();
        let mut unclassified: Vec<String> = Vec::new();
        let mut malformed: Vec<String> = Vec::new();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let rel = file
                .strip_prefix(&workspace)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            let crate_rank = [
                "crates/nml-core/",
                "crates/nml-validate/",
                "nml-cli/",
                "crates/nml-lsp/",
            ]
            .iter()
            .position(|p| rel.starts_with(p))
            .unwrap_or(9);
            for (name, rhs) in declarations(&text) {
                if !bound_shaped(name) {
                    continue;
                }
                let key = format!("{rel}::{name}");
                let Some((line, block)) = doc_block_above(&text, name) else {
                    unclassified.push(key);
                    continue;
                };
                let last = block.last().map(String::as_str).unwrap_or("");
                if let Some(why) = last.strip_prefix("UNPUBLISHED: ") {
                    assert!(
                        !why.is_empty(),
                        "{key}: an unpublished bound needs a reason"
                    );
                    unpublished.push(Unpublished {
                        path: leak(&rel),
                        name: leak(name),
                        why: leak(why),
                    });
                } else if last.starts_with("LIMIT:") {
                    match parse_limit_line(last) {
                        Some((reach, guards, surface, shown, what)) => published.push((
                            crate_rank,
                            line,
                            Limit {
                                path: leak(&rel),
                                name: leak(name),
                                expr: leak(&rhs),
                                shown: leak(&shown),
                                reach,
                                guards,
                                surface,
                                what: leak(&what),
                            },
                        )),
                        None => malformed.push(format!("{key}: {last}")),
                    }
                } else {
                    unclassified.push(key);
                }
            }
        }
        assert!(
            malformed.is_empty(),
            "LIMIT: line(s) the census cannot parse (`reach=<r> guards=<g> surface=<s> \
             shown=\"<v>\" — <what>`): {malformed:#?}"
        );
        assert!(
            unclassified.is_empty(),
            "new bound(s) with no classification line — end the declaration's doc comment \
             with `LIMIT: reach=… guards=… surface=… shown=\"…\" — <what>` or `UNPUBLISHED: \
             <why>`: {unclassified:#?}"
        );
        published.sort_by(|a, b| {
            let rank = |r: Reach| match r {
                Reach::Content => 0,
                Reach::Peer => 1,
                Reach::Internal => 2,
            };
            (rank(a.2.reach), a.0, a.2.path, a.1).cmp(&(rank(b.2.reach), b.0, b.2.path, b.1))
        });
        unpublished.sort_by(|a, b| (a.path, a.name).cmp(&(b.path, b.name)));
        (
            published.into_iter().map(|(_, _, l)| l).collect(),
            unpublished,
        )
    }

    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }

    /// The generated file's text, from the declarations.
    fn generate_table() -> String {
        let (published, unpublished) = classified_declarations();
        let mut out = String::from(
            "// GENERATED by the census from every `LIMIT:` / `UNPUBLISHED:` doc line in the tree\n\
             // (`NML_UPDATE_GOLDEN=1 cargo test -p nml-cli -- \
             limits::tests::the_generated_table_is_fresh`);\n\
             // never edit by hand — classify a bound beside its declaration.\n\n",
        );
        out.push_str("pub const LIMITS: &[Limit] = &[\n");
        for l in &published {
            let reach = match l.reach {
                Reach::Content => "Content",
                Reach::Peer => "Peer",
                Reach::Internal => "Internal",
            };
            let guards = match l.guards {
                Guards::Work => "Work",
                Guards::Memory => "Memory",
                Guards::Output => "Output",
                Guards::Domain => "Domain",
            };
            let surface = match l.surface {
                Surface::Kernel => "Kernel",
                Surface::Cli => "Cli",
                Surface::Editor => "Editor",
            };
            out.push_str(&format!(
                "    Limit {{\n        path: {:?},\n        name: {:?},\n        expr: {:?},\n        \
                 shown: {:?},\n        reach: Reach::{reach},\n        guards: Guards::{guards},\n        \
                 surface: Surface::{surface},\n        what: {:?},\n    }},\n",
                l.path, l.name, l.expr, l.shown, l.what
            ));
        }
        out.push_str("];\n\npub const UNPUBLISHED: &[Unpublished] = &[\n");
        for u in &unpublished {
            out.push_str(&format!(
                "    Unpublished {{\n        path: {:?},\n        name: {:?},\n        why: {:?},\n    }},\n",
                u.path, u.name, u.why
            ));
        }
        out.push_str("];\n");
        out
    }

    /// THE ratchet (claim 6), at the declaration: every bound-shaped
    /// declaration in the tree ends its doc comment in a `LIMIT:` line
    /// (published, with its value as a human reads it) or an
    /// `UNPUBLISHED:` line (with a reason) — the census refuses a new
    /// `MAX_*` until someone decides which — and the committed table is
    /// exactly what those lines generate: a changed value, a reworded
    /// line, a moved declaration or a hand edit of the table fails this
    /// test until `NML_UPDATE_GOLDEN=1` regenerates it. Hand-maintained
    /// limit tables are exactly the artefact that rots; this one is data.
    #[test]
    fn the_generated_table_is_fresh() {
        let generated = generate_table();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/limits_table.rs");
        if std::env::var_os("NML_UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &generated).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap();
        if committed != generated {
            // The first line that differs, both ways: the reviewer sees
            // WHICH declaration moved before regenerating.
            let (n, want, have) = committed
                .lines()
                .zip(generated.lines())
                .enumerate()
                .find(|(_, (c, g))| c != g)
                .map(|(i, (c, g))| (i + 1, g.to_string(), c.to_string()))
                .unwrap_or_else(|| {
                    let n = committed.lines().count().min(generated.lines().count()) + 1;
                    (
                        n,
                        generated.lines().nth(n - 1).unwrap_or("<end>").to_string(),
                        committed.lines().nth(n - 1).unwrap_or("<end>").to_string(),
                    )
                });
            panic!(
                "nml-cli/src/limits_table.rs is stale at line {n}:\n  declared:  {want}\n  \
                 committed: {have}\nregenerate it with `NML_UPDATE_GOLDEN=1 cargo test -p nml-cli \
                 -- limits::tests::the_generated_table_is_fresh` and review the diff (that IS \
                 the change)"
            );
        }
        assert!(
            LIMITS.len() > 30,
            "the census found only {} published bounds — the scanner is broken, not the tree",
            LIMITS.len()
        );
    }

    /// The recognizer contract, executed: `static`, a `const fn` and a
    /// name from every word of the vocabulary are seen; a substring
    /// (`CAPACITY`) and a `let` are not (a census that missed a `static`,
    /// a `const fn` or `DEPTH_CEILING` would publish nothing about them).
    #[test]
    fn the_recognizer_sees_static_const_fn_and_the_whole_vocabulary() {
        // Joined at run time: a declaration at a line start of THIS
        // file would be a hit for the census scanning it.
        let text = [
            "pub static MAX_STEALTH_BOUND: usize = 1;",
            "pub const DEPTH_CEILING: u32 = 2;",
            "pub const fn stealth_cap() -> usize {",
            "    3",
            "}",
            "const CAPACITY: usize = 4;",
            "let MAX_LOCAL: usize = 5;",
            "pub(crate) static QUOTA_FLOOR: u8 = 6;",
        ]
        .join("\n");
        let text = text.as_str();
        let seen: BTreeSet<&str> = declarations(text)
            .into_iter()
            .filter(|(name, _)| bound_shaped(name))
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            seen,
            [
                "MAX_STEALTH_BOUND",
                "DEPTH_CEILING",
                "stealth_cap",
                "QUOTA_FLOOR"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            declarations(text)
                .iter()
                .find(|(n, _)| *n == "stealth_cap")
                .map(|(_, sig)| sig.as_str()),
            Some("fn stealth_cap() -> usize")
        );
    }

    /// PIN CENSUS: every published bound is NAMED by a test — an
    /// identifier use in a `tests/` file or a `#[cfg(test)]` module,
    /// comments and strings blanked — or waived in [`WAIVERS`] with a
    /// reason. A pin that spells the literal (`16`) instead of the
    /// constant is exactly what this forces into the open (a tree once
    /// had 17 published bounds no test named, `MAX_STACK_DEPTH` — the RFC
    /// 0019 hard cap — among them).
    #[test]
    fn every_published_bound_is_named_by_a_test_or_waived() {
        let regions = test_regions();
        assert!(
            regions.len() > 20,
            "found only {} test regions",
            regions.len()
        );
        let waived: BTreeSet<String> = WAIVERS
            .iter()
            .map(|w| {
                assert!(!w.why.is_empty(), "{}::{} needs a reason", w.path, w.name);
                format!("{}::{}", w.path, w.name)
            })
            .collect();
        let mut unnamed: Vec<String> = Vec::new();
        let mut needless: Vec<String> = Vec::new();
        for limit in LIMITS {
            let key = format!("{}::{}", limit.path, limit.name);
            let unique = LIMITS.iter().filter(|l| l.name == limit.name).count() == 1;
            let own_tests = format!(
                "{}/tests/",
                limit.path.rsplit_once('/').map_or("", |(dir, _)| dir)
            );
            let named = regions.iter().any(|(file, region)| {
                names_identifier(region, limit.name)
                    && (unique || file.ends_with(limit.path) || file.contains(&own_tests))
            });
            match (named, waived.contains(&key)) {
                (false, false) => unnamed.push(key),
                (true, true) => needless.push(key),
                _ => {}
            }
        }
        assert!(
            unnamed.is_empty(),
            "published bound(s) no test names — add a pin that uses the constant, or a WAIVER \
             with a reason: {unnamed:#?}"
        );
        assert!(
            needless.is_empty(),
            "waived bound(s) a test now names — drop the waiver: {needless:#?}"
        );
        for w in WAIVERS {
            assert!(
                LIMITS.iter().any(|l| l.path == w.path && l.name == w.name),
                "{}::{} is waived but not published",
                w.path,
                w.name
            );
            assert!(
                LIMITS
                    .iter()
                    .any(|l| l.path == w.path && l.name == w.name && l.guards == Guards::Domain),
                "{}::{}: only a domain ceiling may be waived",
                w.path,
                w.name
            );
        }
    }

    /// The editor indexes a file under the bound the CLI checks a target
    /// under: one size, both front ends (the two constants live in two
    /// crates that cannot see each other; the table sees both).
    #[test]
    fn the_editor_index_bound_is_the_cli_target_bound() {
        let expr = |name: &str| {
            LIMITS
                .iter()
                .find(|l| l.name == name)
                .unwrap_or_else(|| panic!("{name} is published"))
                .expr
        };
        assert_eq!(expr("MAX_INDEX_BYTES"), expr("MAX_TARGET_BYTES"));
    }

    /// The two per-kind input caps are published bounds the census sees
    /// (`MAX_MANIFEST_BYTES`, `MAX_SOURCE_BYTES` — no hand-written rows,
    /// no copy) and the kernel's selector reads exactly them.
    #[test]
    fn input_caps_are_census_rows_read_from_the_kernel() {
        use nml_validate::workspace::{InputKind, MAX_MANIFEST_BYTES, MAX_SOURCE_BYTES, input_cap};
        let row = |name: &str| {
            LIMITS
                .iter()
                .find(|l| l.name == name)
                .unwrap_or_else(|| panic!("{name} is not a census row"))
        };
        assert_eq!(row("MAX_MANIFEST_BYTES").shown, "256 KiB");
        assert_eq!(row("MAX_SOURCE_BYTES").shown, "4 MiB");
        assert_eq!(input_cap(InputKind::Manifest), MAX_MANIFEST_BYTES);
        assert_eq!(input_cap(InputKind::ProjectConfig), MAX_MANIFEST_BYTES);
        assert_eq!(input_cap(InputKind::Source), MAX_SOURCE_BYTES);
        assert!(
            UNPUBLISHED
                .iter()
                .all(|u| u.name != "MAX_MANIFEST_BYTES" && u.name != "MAX_SOURCE_BYTES"),
            "published, not hidden"
        );
    }

    /// The value of a declaration's right-hand side, by a grammar that
    /// covers every bound in the tree: integer literals (`_` allowed),
    /// `*` and `+` (products bind tighter), `as <type>` (a widening the
    /// value ignores), `<int>::MAX`, and a bound named by a sibling
    /// declaration (`16 * MAX_ENTRIES`, `crate::diagnostic::MAX_ERRORS`) —
    /// resolved through the table by bare name (unique, or the same
    /// file's). `None` when the RHS says something else.
    fn evaluate(expr: &str, own_path: &str) -> Option<u128> {
        let tokens: Vec<&str> = expr.split_whitespace().collect();
        let mut sum: u128 = 0;
        let mut product: u128 = 1;
        let mut i = 0;
        while i < tokens.len() {
            let tok = tokens[i];
            match tok {
                "*" => {}
                "+" => {
                    sum = sum.checked_add(product)?;
                    product = 1;
                }
                "as" => i += 1,
                _ => {
                    let value = if let Ok(v) = tok.replace('_', "").parse::<u128>() {
                        v
                    } else if let Some(ty) = tok.strip_suffix("::MAX") {
                        match ty {
                            "u32" => u32::MAX as u128,
                            "u64" => u64::MAX as u128,
                            "usize" => usize::MAX as u128,
                            "u128" => u128::MAX,
                            _ => return None,
                        }
                    } else {
                        let name = tok.rsplit("::").next()?;
                        let mut rows = LIMITS.iter().filter(|l| l.name == name);
                        let row = match (rows.next(), rows.next()) {
                            (Some(row), None) => row,
                            _ => LIMITS
                                .iter()
                                .find(|l| l.name == name && l.path == own_path)?,
                        };
                        evaluate(row.expr, row.path)?
                    };
                    product = product.checked_mul(value)?;
                }
            }
            i += 1;
        }
        sum.checked_add(product)
    }

    /// `shown` is the ONE hand-written field of a published row (the
    /// value as a human reads it): every other field is generated from
    /// the declaration, and the freshness test compares the RHS
    /// verbatim — but a wrong rendering typed once publishes forever.
    /// Pinned: each evaluable declaration's `shown` is its value in the
    /// table's vocabulary — decimal, `human_bytes`, `N bytes`, an
    /// approximation `~x.ye+N <unit>`, or `10^k - 1` — and the rows the
    /// grammar cannot evaluate are named, so a new shape is a decision.
    #[test]
    fn shown_is_the_declared_value_as_a_human_reads_it() {
        let mut wrong: Vec<String> = Vec::new();
        let mut unevaluated: Vec<String> = Vec::new();
        for limit in LIMITS {
            let Some(value) = evaluate(limit.expr, limit.path) else {
                unevaluated.push(format!("{}::{} = {}", limit.path, limit.name, limit.expr));
                continue;
            };
            let shown = limit.shown;
            let accepted = shown == value.to_string()
                || shown == format!("{value} bytes")
                || u64::try_from(value)
                    .is_ok_and(|v| shown == nml_validate::workspace::human_bytes(v))
                || shown
                    .strip_prefix('~')
                    .and_then(|rest| rest.split_once(' '))
                    .is_some_and(|(approx, _unit)| {
                        approx.replace("e+", "e") == format!("{:.1e}", value as f64)
                    })
                || {
                    let digits = (value + 1).to_string();
                    digits.starts_with('1')
                        && digits[1..].bytes().all(|b| b == b'0')
                        && shown == format!("10^{} - 1", digits.len() - 1)
                };
            if !accepted {
                wrong.push(format!(
                    "{}::{}: declared {} = {value}, shown {shown:?}",
                    limit.path, limit.name, limit.expr
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "a `shown=` rendering disagrees with its declaration: {wrong:#?}"
        );
        assert!(
            unevaluated.is_empty(),
            "declaration(s) the `shown` check cannot evaluate — extend `evaluate` or \
             respell the RHS: {unevaluated:#?}"
        );
    }

    /// The legend names the reaches the table actually prints, and the
    /// class the closing line counts.
    ///
    /// It used to teach a vocabulary the data does not use: it named
    /// `operator` — a class with no member in this tree, by design — and
    /// never named `internal`, which is the class behind the footer's
    /// "N internal bound(s) are deliberately unpublished". A reader
    /// scanning for an `[operator]` group found none and had no legend
    /// entry for the word the footer used.
    #[test]
    fn the_legend_names_the_classes_the_table_uses() {
        let printed: std::collections::BTreeSet<&str> =
            LIMITS.iter().map(|l| l.reach.label()).collect();
        let legend = crate::limits::legend();
        for class in &printed {
            assert!(
                legend.contains(*class),
                "the legend does not name `{class}`, which the table prints: {legend}"
            );
        }
        assert!(
            !printed.contains("operator"),
            "a published bound is operator-reachable now — the legend has to name it"
        );
        assert!(
            legend.contains("internal") && !legend.contains("only the operator"),
            "the legend must name the unpublished class and not an empty one: {legend}"
        );
        // Every published row's reach is one the legend explains.
        assert_eq!(
            printed,
            std::collections::BTreeSet::from(["content", "peer"]),
            "a new published reach needs a legend clause"
        );
    }
}
