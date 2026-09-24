//! The module-arrow ratchet: every dependency arrow one PRODUCT module of
//! this crate draws to another, pinned. A new arrow — above all a new
//! upward one, which is how a cycle arrives — fails here and is added on
//! purpose, with the layering it respects said out loud, instead of
//! landing unnoticed in a `use` line. RFC 0026 decision 4 recorded the
//! crate's cycles and the moves that break them; this test is what keeps
//! a broken one broken.
//!
//! The walk is `syn`'s (the parser rustc's own token rules are mirrored
//! by): every `use` tree and every qualified path in type or expression
//! position that begins with `crate::`, `super::` or `self::`, resolved
//! against the file's own module, then reduced to the MODULE it names — a
//! crate-level module (`package`, `fs`; everything under `src/fs/` is the
//! one leaf module `fs`), or a child of `workspace` (`workspace::diag`).
//! Skipped: `#[cfg(test)]` items and modules, `tests/` directories,
//! `test_support` (test-only by feature), doc comments (a link is not an
//! arrow), and macro bodies (opaque to the visitor — an arrow spelled ONLY
//! inside one is not seen; every arrow the crate draws today is a `use`
//! or a qualified path outside a macro). Parent–child arrows in either
//! direction (`workspace::diag -> workspace`, `workspace -> workspace::paths`)
//! are the module tree's own shape and are not pinned; every other arrow is.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// Every pinned arrow, `from -> to`, sorted. The layering the list states:
/// `fs` is THE leaf — the crate's one filesystem oracle, reader and
/// listing rule — and draws no arrow at all; `file_names` and `glob` are
/// the other leaves; `package` reads `directives`, `loader`, `glob`, `fs`
/// (a package directory's bounded reads) and the leaves; `store` reads
/// `package`, `glob` and `fs` (the `current` pointer); `workspace::*`
/// reads `package`, `schema`, `store`, `directives`, `fs`, the leaves and
/// its own siblings — and the ONE cycle the crate still carries is
/// `workspace::discover <-> workspace::diag` (the walk mints rows through
/// the code table while the table reads the walk's facts; RFC 0026
/// decision 4, resolution planned: the walk records typed facts, `notes`
/// renders them). Breaking it removes the `workspace::discover ->
/// workspace::diag` row here.
const ALLOWED: &[(&str, &str)] = &[
    ("directives", "package"),
    ("loader", "schema"),
    ("package", "file_names"),
    // the bounded reader and the listing rule: a package DIRECTORY's
    // manifest and declared sources are read exactly as the walk reads a
    // live package's, beneath the directory, under the same two bounds
    ("package", "fs"),
    ("package", "glob"),
    ("package", "loader"),
    ("package", "schema"),
    // the `current` pointer, read under its own bound
    ("store", "fs"),
    // the store judges slot names by the glob leaf's plain-name rule
    ("store", "glob"),
    ("store", "package"),
    ("workspace", "file_names"),
    ("workspace::claims", "fs"),
    ("workspace::claims", "glob"),
    ("workspace::claims", "package"),
    ("workspace::claims", "schema"),
    ("workspace::claims", "workspace::paths"),
    ("workspace::diag", "file_names"),
    ("workspace::diag", "fs"),
    ("workspace::diag", "glob"),
    ("workspace::diag", "package"),
    ("workspace::diag", "workspace::claims"),
    // The one cycle (with `workspace::discover -> workspace::diag` below).
    ("workspace::diag", "workspace::discover"),
    ("workspace::diag", "workspace::paths"),
    ("workspace::discover", "file_names"),
    ("workspace::discover", "fs"),
    ("workspace::discover", "glob"),
    ("workspace::discover", "package"),
    ("workspace::discover", "schema"),
    ("workspace::discover", "workspace::claims"),
    ("workspace::discover", "workspace::diag"),
    ("workspace::discover", "workspace::grants"),
    ("workspace::discover", "workspace::paths"),
    ("workspace::grants", "glob"),
    ("workspace::grants", "workspace::claims"),
    ("workspace::mock", "fs"),
    // scripted paths fold through the same `split_absolute` the kernel uses
    ("workspace::mock", "workspace::paths"),
    ("workspace::notes", "workspace::claims"),
    ("workspace::notes", "workspace::diag"),
    ("workspace::notes", "workspace::discover"),
    ("workspace::notes", "workspace::paths"),
    ("workspace::paths", "file_names"),
    ("workspace::paths", "fs"),
    ("workspace::paths", "glob"),
    ("workspace::vocabulary", "directives"),
    ("workspace::vocabulary", "file_names"),
    ("workspace::vocabulary", "package"),
    ("workspace::vocabulary", "workspace::claims"),
    ("workspace::vocabulary", "workspace::discover"),
    ("workspace::vocabulary", "workspace::paths"),
];

/// The crate's top-level product modules, from `lib.rs`.
const TOP: &[&str] = &[
    "directives",
    "file_names",
    "glob",
    "loader",
    "package",
    "fs",
    "schema",
    "store",
    "workspace",
];

/// `workspace`'s child modules, from `workspace/mod.rs`.
const WORKSPACE_CHILDREN: &[&str] = &[
    "claims",
    "diag",
    "discover",
    "grants",
    "mock",
    "notes",
    "paths",
    "vocabulary",
];

/// A file's module, from its path under `src/`: `None` for the crate root
/// and for anything the ratchet does not judge (tests, test support).
fn module_of(rel: &Path) -> Option<Vec<String>> {
    let mut parts: Vec<String> = rel
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.iter().any(|p| p == "tests") || parts.first().is_some_and(|p| p == "test_support") {
        return None;
    }
    if parts.last().is_some_and(|p| p == "mod" || p == "lib") {
        parts.pop();
    }
    if parts.is_empty() {
        return None;
    }
    // Everything under `fs/` is the crate's one filesystem leaf.
    if parts.len() > 1 && parts[0] == "fs" {
        parts.truncate(1);
    }
    Some(parts)
}

/// The judged module a resolved path names, or `None` when it names an
/// item of the crate root, an unknown module, or nothing judged.
fn target_module(resolved: &[String]) -> Option<String> {
    let first = resolved.first()?;
    if !TOP.contains(&first.as_str()) {
        return None;
    }
    if first == "workspace" {
        if let Some(second) = resolved.get(1) {
            if WORKSPACE_CHILDREN.contains(&second.as_str()) {
                return Some(format!("workspace::{second}"));
            }
        }
    }
    Some(first.clone())
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && {
            let mut has_test = false;
            let _ = a.parse_nested_meta(|meta| {
                if meta.path.is_ident("test") {
                    has_test = true;
                } else if meta.path.is_ident("any") || meta.path.is_ident("all") {
                    // `cfg(any(test, feature = "test-support"))`
                    let content;
                    syn::parenthesized!(content in meta.input);
                    let inner: proc_macro2::TokenStream = content.parse()?;
                    if inner.to_string().split(',').any(|t| t.trim() == "test") {
                        has_test = true;
                    }
                } else if meta.input.peek(syn::Token![=]) {
                    let _ = meta.value()?.parse::<syn::Lit>()?;
                }
                Ok(())
            });
            has_test
        }
    })
}

struct Arrows<'a> {
    module: &'a [String],
    out: &'a mut BTreeSet<(String, String)>,
}

impl Arrows<'_> {
    /// Resolve `crate::…`, `super::…`, `self::…` against the file's module.
    fn resolve(&self, segments: &[String]) -> Option<Vec<String>> {
        let mut it = segments.iter();
        let mut base: Vec<String> = match it.next()?.as_str() {
            "crate" => Vec::new(),
            "self" => self.module.to_vec(),
            "super" => {
                let mut m = self.module.to_vec();
                m.pop();
                m
            }
            _ => return None,
        };
        for seg in it {
            if seg == "super" {
                base.pop();
            } else {
                base.push(seg.clone());
            }
        }
        Some(base)
    }

    fn note(&mut self, segments: &[String]) {
        let Some(resolved) = self.resolve(segments) else {
            return;
        };
        let Some(to) = target_module(&resolved) else {
            return;
        };
        let from = self.module.join("::");
        // Parent–child arrows are the module tree's own shape.
        if to == from
            || from.starts_with(&format!("{to}::"))
            || to.starts_with(&format!("{from}::"))
        {
            return;
        }
        self.out.insert((from, to));
    }

    fn use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.use_tree(&p.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                prefix.push(n.ident.to_string());
                self.note(prefix);
                prefix.pop();
            }
            syn::UseTree::Rename(r) => {
                prefix.push(r.ident.to_string());
                self.note(prefix);
                prefix.pop();
            }
            syn::UseTree::Glob(_) => self.note(prefix),
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.use_tree(t, prefix);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Arrows<'_> {
    fn visit_item(&mut self, i: &'ast syn::Item) {
        let attrs: &[syn::Attribute] = match i {
            syn::Item::Const(x) => &x.attrs,
            syn::Item::Enum(x) => &x.attrs,
            syn::Item::Fn(x) => &x.attrs,
            syn::Item::Impl(x) => &x.attrs,
            syn::Item::Macro(x) => &x.attrs,
            syn::Item::Mod(x) => &x.attrs,
            syn::Item::Static(x) => &x.attrs,
            syn::Item::Struct(x) => &x.attrs,
            syn::Item::Trait(x) => &x.attrs,
            syn::Item::Type(x) => &x.attrs,
            syn::Item::Use(x) => &x.attrs,
            _ => &[],
        };
        if is_cfg_test(attrs) {
            return;
        }
        syn::visit::visit_item(self, i);
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        self.use_tree(&i.tree, &mut prefix);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segments: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        self.note(&segments);
        syn::visit::visit_path(self, p);
    }
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("source dir lists")
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            sources(&entry, out);
        } else if entry.extension().is_some_and(|x| x == "rs") {
            out.push(entry);
        }
    }
}

#[test]
fn every_module_arrow_is_pinned() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let mut arrows = BTreeSet::new();
    for file in &files {
        let rel = file.strip_prefix(&src).expect("under src");
        let Some(module) = module_of(rel) else {
            continue;
        };
        let text = std::fs::read_to_string(file).expect("source readable");
        let parsed = syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", rel.display()));
        let mut walk = Arrows {
            module: &module,
            out: &mut arrows,
        };
        walk.visit_file(&parsed);
    }
    let pinned: BTreeSet<(String, String)> = ALLOWED
        .iter()
        .map(|(f, t)| (f.to_string(), t.to_string()))
        .collect();
    let new: Vec<String> = arrows
        .difference(&pinned)
        .map(|(f, t)| format!("    (\"{f}\", \"{t}\"),"))
        .collect();
    let gone: Vec<String> = pinned
        .difference(&arrows)
        .map(|(f, t)| format!("    (\"{f}\", \"{t}\"),"))
        .collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "module arrows changed — add each new arrow to ALLOWED on purpose, naming the layering \
         it respects, and drop each arrow that is gone:\nNEW:\n{}\nGONE:\n{}\nMEASURED (whole set):\n{}",
        new.join("\n"),
        gone.join("\n"),
        arrows
            .iter()
            .map(|(f, t)| format!("    (\"{f}\", \"{t}\"),"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
