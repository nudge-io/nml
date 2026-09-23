//! The source ratchet (A15, E28).
//!
//!
//! Layer A has no ambient authority: every kernel source under
//! `src/workspace/` — recursively; the top-level `tests/` directory
//! excepted — reaches `std`/`core`/`alloc`
//! only through the roots in [`ALLOWED_STD_ROOTS`], never names a door
//! crate, a door method, a door macro, an `unsafe` block, an `extern`
//! block or a module redirection. Since r69b the scan is an AST walk
//! (`syn`, the parser rustc's own token rules are mirrored by): a comment,
//! a raw identifier, a bidi mark between tokens, a turbofish, a qualified
//! path or a `use` group is seen exactly as the compiler sees it — the
//! 453-line lexer-lite it replaces re-learned each of those spellings one
//! certification round at a time (r51–r60). What the walk PROVES, and no
//! more: a spelling in the kernel's own text. A door that arrives by a
//! dependency crate's re-export or a build script is not seen by it; the
//! oracle is the only window on the world, and the ratchet is what keeps
//! a future "just canonicalize the whole thing" out of the kernel.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use syn::visit::Visit;

/// The standard-library roots a KERNEL file may reach (`std::<root>`,
/// `core::<root>`, `alloc::<root>`): exactly the data-structure and
/// formatting modules the kernel uses today. Widening this list is a
/// reviewed change. Nothing on it is a door to the world, with one
/// exception the walk names separately: `std::path::absolute`, which
/// resolves against the working directory ([`DOOR_METHODS`]).
const ALLOWED_STD_ROOTS: &[&str] = &["cell", "collections", "error", "ffi", "fmt", "path", "sync"];

/// Crates that ARE the world: the raw syscall bindings (`libc`, and
/// `rustix` — the oracle's safe binding, E35), the oracle's Windows
/// realpath (`dunce`), the store's home-directory lookup (`dirs`).
/// Naming any item of them, importing them, or `extern crate`-ing them
/// is a door.
const DOOR_CRATES: &[&str] = &["libc", "rustix", "dunce", "dirs"];

/// Doors by NAME, wherever they appear as the last segment of a path of
/// two or more segments (`Path::exists`, `s::fs::read_to_string`,
/// `<Path>::metadata`, `mystd::fs::read`) or as a method call
/// (`p.exists()`): the `Path`/`File`/`env` doors of `std`, however the
/// module is spelled — through an alias, a raw identifier, another
/// crate that re-exports them. The injected oracle's own names (`child`,
/// `list_dir`, `resolve_symlink`) are deliberately NOT here.
const DOOR_METHODS: &[&str] = &[
    "canonicalize",
    "exists",
    "is_dir",
    "is_file",
    "is_symlink",
    "metadata",
    "symlink_metadata",
    "read_dir",
    "read_link",
    "try_exists",
    "read_to_string",
    "read",
    "write",
    "create",
    "open",
    "current_dir",
    "temp_dir",
    "home_dir",
    "absolute",
    "var",
    "var_os",
    "args",
    "args_os",
];

/// The cwd/env/home doors as BARE calls (`current_dir()` after a `use`
/// the scan also sees): a single-segment path naming one of these is a
/// door on its own.
const DOOR_FREE_FUNCTIONS: &[&str] = &["current_dir", "temp_dir", "home_dir", "absolute"];

/// The real backends, named in expression or type position (`StdFs`,
/// `StdFs::default()`, `WasiFs::new(..)`, `OverlayFs::new(..)`): a
/// kernel file that constructs one holds ambient authority whatever it
/// calls on it, so the oracle's own types are doors outside `fs/` — the
/// `pub use` re-export in `mod.rs` is a `use`, not a construction (r70).
const DOOR_BACKENDS: &[&str] = &["StdFs", "WasiFs", "OverlayFs"];

/// Source inclusion and build-time environment reads.
const DOOR_MACROS: &[&str] = &[
    "include",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
];

/// An identifier as rustc resolves it: a raw identifier (`r#std`) is its
/// identifier.
fn ident(i: &syn::Ident) -> String {
    i.to_string().trim_start_matches("r#").to_string()
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| ident(&s.ident)).collect()
}

/// Pass one: the crate aliases a file declares (`extern crate std as s`,
/// `use std as s`, `use {core::fmt, alloc as a}`), so that pass two can
/// read `s::fs::…` as `std::fs::…` wherever the alias sits in the file.
#[derive(Default)]
struct Aliases(HashSet<String>);

fn is_std_crate(name: &str) -> bool {
    matches!(name, "std" | "core" | "alloc")
}

impl<'ast> Visit<'ast> for Aliases {
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        if let Some((_, alias)) = &i.rename {
            if is_std_crate(&ident(&i.ident)) {
                self.0.insert(ident(alias));
            }
        }
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        fn walk(aliases: &mut HashSet<String>, tree: &syn::UseTree, depth: usize) {
            match tree {
                syn::UseTree::Path(p) => walk(aliases, &p.tree, depth + 1),
                syn::UseTree::Rename(r) => {
                    if depth == 0 && is_std_crate(&ident(&r.ident)) {
                        aliases.insert(ident(&r.rename));
                    }
                }
                syn::UseTree::Group(g) => {
                    for t in &g.items {
                        walk(aliases, t, depth);
                    }
                }
                syn::UseTree::Name(_) | syn::UseTree::Glob(_) => {}
            }
        }
        walk(&mut self.0, &i.tree, 0);
    }
}

/// Pass two: every door, as a sentence naming it.
struct Walk<'a> {
    aliases: &'a HashSet<String>,
    hits: Vec<String>,
}

impl Walk<'_> {
    fn hit(&mut self, what: String) {
        self.hits.push(what);
    }

    fn is_std(&self, root: &str) -> bool {
        is_std_crate(root) || self.aliases.contains(root)
    }

    /// One path — an expression, a type, a pattern, a `use` leaf — as its
    /// segments. `ctx` names where it was seen for the sentence.
    fn check_path(&mut self, segs: &[String], ctx: &str) {
        let Some(root) = segs.first() else {
            return;
        };
        let spelled = segs.join("::");
        if DOOR_CRATES.contains(&root.as_str()) && (segs.len() >= 2 || ctx == "use") {
            self.hit(format!("{ctx} `{spelled}` (a door crate)"));
            return;
        }
        if let Some(last) = segs.last() {
            if segs.len() >= 2 && DOOR_METHODS.contains(&last.as_str()) {
                self.hit(format!("{ctx} `{spelled}` (a door by name)"));
                return;
            }
            if segs.len() == 1 && ctx == "path" && DOOR_FREE_FUNCTIONS.contains(&last.as_str()) {
                self.hit(format!("{ctx} `{spelled}` (a bare cwd/env door)"));
                return;
            }
            if ctx != "use" && segs.iter().any(|s| DOOR_BACKENDS.contains(&s.as_str())) {
                self.hit(format!(
                    "{ctx} `{spelled}` (a real backend named outside fs/)"
                ));
                return;
            }
        }
        if !self.is_std(root) {
            return;
        }
        match segs.get(1) {
            Some(module) if ALLOWED_STD_ROOTS.contains(&module.as_str()) => {}
            Some(module) => self.hit(format!(
                "{ctx} `{spelled}` (std root `{module}` is not in the allow-list)"
            )),
            None => self.hit(format!("{ctx} bare `{spelled}` (a crate alias or a glob)")),
        }
    }

    fn check_use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(ident(&p.ident));
                self.check_use_tree(&p.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                prefix.push(ident(&n.ident));
                self.check_path(prefix, "use");
                prefix.pop();
            }
            syn::UseTree::Rename(r) => {
                prefix.push(ident(&r.ident));
                self.check_path(prefix, "use");
                prefix.pop();
            }
            syn::UseTree::Glob(_) => {
                // A glob under a std root reaches every item under it —
                // `absolute` under `path` included.
                if prefix.first().is_some_and(|root| self.is_std(root)) {
                    self.hit(format!("use `{}::*` (a glob under std)", prefix.join("::")));
                } else {
                    self.check_path(prefix, "use");
                }
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.check_use_tree(t, prefix);
                }
            }
        }
    }

    /// A door-typed segment behind a qualified self (`<Path>::exists`,
    /// `<T as Trait>::metadata`): the path holds the trailing segments
    /// only, so the two-segment rule cannot see it.
    fn check_qualified(&mut self, qself: Option<&syn::QSelf>, path: &syn::Path) {
        if qself.is_none() {
            return;
        }
        if let Some(last) = path.segments.last() {
            let name = ident(&last.ident);
            if DOOR_METHODS.contains(&name.as_str()) {
                self.hit(format!("qualified path `<_>::{name}` (a door by name)"));
            }
        }
    }

    /// The token-level fallback for a macro body that is not a list of
    /// expressions: fail-closed on any door-shaped identifier.
    fn scan_tokens(&mut self, tokens: proc_macro2::TokenStream, in_macro: &str) {
        for tt in tokens {
            match tt {
                proc_macro2::TokenTree::Group(g) => self.scan_tokens(g.stream(), in_macro),
                proc_macro2::TokenTree::Ident(i) => {
                    let s = i.to_string();
                    let s = s.trim_start_matches("r#");
                    if DOOR_METHODS.contains(&s)
                        || DOOR_CRATES.contains(&s)
                        || DOOR_FREE_FUNCTIONS.contains(&s)
                        || DOOR_BACKENDS.contains(&s)
                        || self.is_std(s)
                    {
                        self.hit(format!(
                            "`{s}` inside `{in_macro}!(..)` (a door in a macro body)"
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Whether an attribute's arguments, as `syn` re-spells them (one space
/// between tokens, string literals quoted), hold the identifier `path` at
/// any depth: `#[cfg_attr(not(test), path = "…")]`.
fn names_path(tokens: &str) -> bool {
    tokens
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|word| word == "path")
}

impl<'ast> Visit<'ast> for Walk<'_> {
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        let name = ident(&i.ident);
        if is_std_crate(&name) || DOOR_CRATES.contains(&name.as_str()) {
            self.hit(format!("extern crate `{name}`"));
        }
        syn::visit::visit_item_extern_crate(self, i);
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        self.check_use_tree(&i.tree, &mut prefix);
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs = segments(p);
        self.check_path(&segs, "path");
        syn::visit::visit_path(self, p);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        self.check_qualified(e.qself.as_ref(), &e.path);
        syn::visit::visit_expr_path(self, e);
    }

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        self.check_qualified(t.qself.as_ref(), &t.path);
        syn::visit::visit_type_path(self, t);
    }

    fn visit_expr_method_call(&mut self, m: &'ast syn::ExprMethodCall) {
        let name = ident(&m.method);
        if DOOR_METHODS.contains(&name.as_str()) {
            self.hit(format!("method call `.{name}(..)` (a door by name)"));
        }
        syn::visit::visit_expr_method_call(self, m);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let name = m
            .path
            .segments
            .last()
            .map(|s| ident(&s.ident))
            .unwrap_or_default();
        if DOOR_MACROS.contains(&name.as_str()) {
            self.hit(format!(
                "macro `{name}!` (source inclusion / build-time env)"
            ));
        }
        if name == "macro_rules" {
            self.hit("`macro_rules!` definition (macro paste)".to_string());
        }
        // A macro's arguments are a token stream `syn` does not walk
        // (r70): `format!("{}", std::fs::read_to_string(p)?)` compiles
        // and was invisible. Parse the body as expressions (every std
        // macro the kernel uses takes them) and walk each; a body that
        // is not expressions is scanned token by token — any door name,
        // door crate, or std root outside the allow-list is a door.
        use syn::punctuated::Punctuated;
        let body = m.parse_body_with(Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated);
        match body {
            Ok(exprs) => {
                for e in &exprs {
                    self.visit_expr(e);
                }
            }
            Err(_) => self.scan_tokens(m.tokens.clone(), &name),
        }
        syn::visit::visit_macro(self, m);
    }

    fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
        if a.path().is_ident("path") {
            self.hit("attribute `#[path]` (module redirection)".to_string());
        } else if a.path().is_ident("cfg_attr")
            && a.meta
                .require_list()
                .is_ok_and(|l| names_path(&l.tokens.to_string()))
        {
            self.hit("attribute `#[cfg_attr(…, path = …)]` (module redirection)".to_string());
        }
        syn::visit::visit_attribute(self, a);
    }

    fn visit_expr_unsafe(&mut self, e: &'ast syn::ExprUnsafe) {
        self.hit("`unsafe` block".to_string());
        syn::visit::visit_expr_unsafe(self, e);
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        if s.unsafety.is_some() {
            self.hit(format!("`unsafe fn {}`", s.ident));
        }
        syn::visit::visit_signature(self, s);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if i.unsafety.is_some() {
            self.hit("`unsafe impl`".to_string());
        }
        syn::visit::visit_item_impl(self, i);
    }

    fn visit_item_foreign_mod(&mut self, i: &'ast syn::ItemForeignMod) {
        self.hit("`extern` block (foreign functions bypass std)".to_string());
        syn::visit::visit_item_foreign_mod(self, i);
    }
}

/// Parse `text` as a file, else as the body of a function (the in-memory
/// rows below are statements as often as items).
fn parse_rust(text: &str) -> Result<syn::File, syn::Error> {
    syn::parse_file(text)
        .or_else(|first| syn::parse_file(&format!("fn __row() {{\n{text}\n}}")).map_err(|_| first))
}

/// The first door in `text`, as a sentence — `None` = clean. Text that is
/// not Rust is REJECTED (the parse error is the sentence): a kernel source
/// always parses, and a smuggled row that fails to parse could not have
/// compiled either.
fn ambient_authority(text: &str) -> Option<String> {
    let file = match parse_rust(text) {
        Ok(file) => file,
        Err(e) => return Some(format!("not valid Rust: {e}")),
    };
    let mut aliases = Aliases::default();
    aliases.visit_file(&file);
    let mut walk = Walk {
        aliases: &aliases.0,
        hits: Vec::new(),
    };
    walk.visit_file(&file);
    walk.hits.into_iter().next()
}

/// Every kernel source under `dir`, recursively, sorted. At the top level
/// the `tests/` directory (this scan and the mock pins) is the ONE
/// exclusion; nothing below the top is excluded — a future
/// `workspace/<sub>/…` is scanned whatever it is named, and a directory
/// or file named `fs` is scanned like any other. The oracle is no longer
/// a child of the kernel at all: it is the crate's leaf (`src/fs/`),
/// outside this scan's root, so a `workspace/fs/` re-created here would
/// be SCANNED, not waved through.
fn kernel_sources(dir: &Path, top: bool, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("workspace dir lists")
        .map(|e| e.expect("entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if entry.is_dir() {
            if !(top && name == "tests") {
                kernel_sources(&entry, false, out);
            }
        } else {
            out.push(entry);
        }
    }
}

/// Layer A has no ambient authority (A15): every kernel source under
/// `src/workspace/` parses and walks clean (see the module comment above
/// for what the walk proves).
#[test]
fn source_ratchet_workspace_has_no_ambient_fs() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workspace");
    let mut sources = Vec::new();
    kernel_sources(&dir, true, &mut sources);
    let mut scanned = Vec::new();
    for entry in sources {
        let text = std::fs::read_to_string(&entry).expect("source reads");
        assert_eq!(
            ambient_authority(&text),
            None,
            "{}: ambient authority outside workspace/fs/ — Layer A observes \
             the world only through the injected PathFs",
            entry.display()
        );
        scanned.push(
            entry
                .strip_prefix(&dir)
                .expect("under the kernel dir")
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    scanned.sort();
    assert!(
        scanned.contains(&"paths.rs".to_string()) && scanned.contains(&"mod.rs".to_string()),
        "the ratchet scanned {scanned:?}"
    );
    assert!(
        !scanned.iter().any(|s| s.starts_with("tests/")),
        "{scanned:?}"
    );
}

/// r52 #2, r69b: the scan recurses — a kernel file in a subdirectory is
/// scanned; the top-level `tests/` DIRECTORY is the only exclusion (a
/// top-level `fs/` directory, an `fs.rs` file, a `sub/fs.rs`, a
/// `sub/fs/x.rs` are all scanned). Over a scratch tree.
#[test]
fn source_ratchet_scans_subdirectories() {
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = std::env::temp_dir().join(format!("nml-ratchet-walk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _guard = Scratch(dir.clone());
    for rel in [
        "mod.rs",
        "fs.rs",
        "fs/mod.rs",
        "fs/disk.rs",
        "paths.rs",
        "tests/mod.rs",
        "sub/mod.rs",
        "sub/fs.rs",
        "sub/fs/x.rs",
        "sub/tests/pin.rs",
        "sub/deeper/x.rs",
    ] {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
    }
    let mut sources = Vec::new();
    kernel_sources(&dir, true, &mut sources);
    let rels: Vec<String> = sources
        .iter()
        .map(|p| {
            p.strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(
        rels,
        [
            "fs/disk.rs",
            "fs/mod.rs",
            "fs.rs",
            "mod.rs",
            "paths.rs",
            "sub/deeper/x.rs",
            "sub/fs/x.rs",
            "sub/fs.rs",
            "sub/mod.rs",
            "sub/tests/pin.rs"
        ]
    );
}

/// The ratchet's own pin (r51 #7, widened every round since; r69b: every
/// row carried into the AST walk): every spelling that walked through an
/// earlier list is rejected on its own, and the kernel's real imports
/// pass. In-memory strings — no scratch file could be mistaken for a
/// kernel source. A row that is not valid Rust is rejected as such (it
/// could not have compiled); the allowed rows that were bare fragments
/// under the text scan are completed to the item they abbreviated.
#[test]
fn source_ratchet_rejects_smuggled_authority() {
    let smuggled = [
        // The six the r51 certifier got past the needle list.
        "let out = std::process::Command::new(\"cat\").output();",
        "let mut line = String::new(); std::io::stdin().read_line(&mut line).ok();",
        "let s = std::net::TcpStream::connect(\"127.0.0.1:80\");",
        "std::os::unix::fs::symlink(\"a\", \"b\").ok();",
        "use std::os::unix::fs::DirBuilderExt;",
        "let text = std :: fs :: read_to_string(path);",
        // What the old list caught, still caught.
        "use std::{env, fs};",
        "let p = std::fs::canonicalize(dir);",
        "let home = std::env::var(\"HOME\");",
        "let cwd = ::std::env::current_dir();",
        "let real = dir.canonicalize();",
        "if path . exists ( ) { }",
        "let target = link.read_link();",
        // The doors that bypass `std`.
        "extern crate libc;",
        "let fd = unsafe { libc::open(p, 0) };",
        // E35: the safe syscall binding is a door like `libc`.
        "extern crate rustix;",
        "let fd = rustix::fs::open(p, rustix::fs::OFlags::RDONLY, rustix::fs::Mode::empty());",
        "use rustix::fs::openat;",
        "extern \"C\" { fn open(path: *const u8) -> i32; }",
        "extern { fn getcwd(b: *mut u8, n: usize) -> *mut u8; }",
        "let t = core::time::Duration::from_secs(1);",
        "let s = alloc::string::String::new();",
        // r52 #2: what walked through the allow-list.
        "extern crate std as s;",
        "let t = s::fs::read_to_string(p);",
        // `mystd` may be `extern crate std as mystd`: the door-by-name
        // rule is crate-agnostic on purpose.
        "let other = mystd::fs::read(p);",
        "let b = s::fs::read(p);",
        "let f = s::fs::File::open(p);",
        "let h = s::env::var(\"HOME\");",
        "use s::env::current_dir;",
        "use s::env::temp_dir;",
        "let c = current_dir();",
        "let d = temp_dir();",
        "let e = s::fs::read_dir(p);",
        "let c = s::fs::canonicalize(p);",
        "let m = s::fs::metadata(p);",
        "let m = s::fs::symlink_metadata(p);",
        "use std::path::absolute;",
        "use std::path as abs; let a = abs::absolute(p);",
        "use std::path::*;",
        "use std::path::{Path, *};",
        "use std::path::{absolute as abs, Path};",
        "if Path::exists(p) {}",
        "let m = Path::metadata(p);",
        "let m = Path::symlink_metadata(p);",
        "let d = Path::read_dir(p);",
        "let l = Path::read_link(p);",
        "let c = Path::canonicalize(p);",
        "let e = Path::try_exists(p);",
        "let d = Path::is_dir(p);",
        "let f = Path::is_file(p);",
        "let s = Path::is_symlink(p);",
        "let m = std::path::Path::metadata(Path::new(p));",
        "#[path = \"../x.rs\"]\nmod m;",
        "#[cfg_attr(not(test), path = \"../x.rs\")]\nmod m;",
        "include!(\"../x.rs\");",
        "const T: &str = include_str!(\"x\");",
        "const B: &[u8] = include_bytes!(\"x\");",
        "macro_rules! m { ($a:ident) => { $a::fs::read_to_string } }",
        // r54 #2: a crate alias without `extern crate`, an empty
        // turbofish, a method as a value, and the `dunce` door.
        "use std as s;",
        "use ::std as s;",
        "use {std as s};",
        "use core as c;",
        "use alloc as a;",
        "let m = Path::metadata::<>(p);",
        "let e = p.exists::<>();",
        "let f = Path::exists; let e = f(p);",
        "let m: fn(&Path) -> Result<Metadata> = Path::symlink_metadata;",
        "let f = dunce::canonicalize; let c = f(p);",
        "use dunce::canonicalize as c;",
        "let s = dunce::simplified(p);",
        // r56 #2: comments, commas and bidi marks between tokens, nested
        // brace groups, and `dirs`.
        "use std/*c*/as s;",
        "use std /* c */ as s;",
        "use std // c\n as s;",
        "use {core::fmt,std as s};",
        "use {core::fmt, std as s};",
        "use std\u{200E}as s;",
        "use std\u{200F}as s;",
        "if p.exists\u{200E}() {}",
        "let m = Path::\u{200E}metadata(p);",
        "let m = Path\u{200F}::metadata(p);",
        "use std::path::{{absolute}};",
        "use std::path::{ {absolute} };",
        "use std::path::{{Path, absolute}};",
        "let h = dirs::home_dir();",
        "use dirs::home_dir as h;",
        // The lexer-lite's own edges, still doors under the walk: a `//`
        // inside a string is text, a raw identifier is not a raw string,
        // a char literal ends.
        "let u = \"http://x\"; use std as s;",
        "let r#type = 1; use std as s;",
        "let c = '\\''; use std as s;",
        "let c = 'x'; use std as s;",
        "let s = r#\"a\"#; use std as s;",
        // Confirmed still caught (r56): the crate-relative and raw forms.
        "use std::{self as s};",
        "use std::fs::{self};",
        "pub use std::fs as f;",
        "extern crate alloc as a;",
        "let d = ::core::time::Duration::ZERO;",
        "const T: [fn(&Path) -> bool; 1] = [Path::exists];",
        "#[cfg(windows)] use std::os::windows::fs::symlink_file;",
        "let f = r#std::fs::read_to_string(p);",
        "let m = p.metadata ::< > ();",
        // r58 #2: a comment splitting a needle.
        "if p.exists/**/() {}",
        "let m = p.metadata/**/();",
        "let d = p.read_dir/* */();",
        "let e = p.try_exists /**/ ();",
        "let out = std/**/::process::Command::new(\"cat\").output();",
        "let out = std /* c */ :: process::Command::new(\"cat\").output();",
        "let s = std // c\n::net::TcpStream::connect(\"127.0.0.1:80\");",
        "let h = std/**/::env::var(\"HOME\");",
        "let h = std/**/::io::stdin();",
        "#/**/[path = \"../x.rs\"]\nmod m;",
        "macro_rules/**/! m { ($a:ident) => { $a::fs::read_to_string } }",
        "include!/**/(\"../x.rs\");",
        "const T: &str = include_str!/**/(\"x\");",
        "#[cfg_attr/**/(not(test), path = \"../x.rs\")]\nmod m;",
        "extern/**/crate libc;",
        "let h = dirs/**/::home_dir();",
        "let c = dunce/**/::canonicalize(p);",
        "let f = Path::exists/**/;",
        "let cwd = current_dir/**/();",
        "let t = temp_dir/**/();",
        "let f = s::fs::File/**/::open(p);",
        "let f = s::fs::read_to_string/**/(p);",
        // Confirmed still caught (r58): the raw-identifier alias, the
        // qualified-path and fn-pointer UFCS forms, and a brace group
        // under `std` that aliases `Path` for a UFCS call behind it.
        "use r#std as s;",
        "let e = <std::path::Path>::exists(p);",
        "let f: fn(&Path) -> bool = std::path::Path::exists;",
        "use core::fmt as f; use std::{ffi, path::Path as P}; let e = P::exists(p);",
        // The comment stripper's own edges: a `/` char literal and a
        // `/*` string are not comment openers.
        "let c = '/'; let p = std::fs::read(p);",
        "let s = \"/*\"; use std as s;",
        // r60: a `'\''` char literal directly followed by a string.
        "let _ = stringify!('\\''\"'\"); let s = \"/* \"; let f = std::fs::read_to_string(p); let t = \" */\";",
        "let _ = stringify!('\\''\"'\"); let s = \"/* \"; use std as s2; let t = \" */\";",
        "let _ = stringify!('\\''\"'\"); let s = \"// \"; let e = p.exists(); let t = \"\\n\";",
        "let _ = stringify!(b'\\''\"'\"); let s = \"/* \"; let f = std::fs::read_to_string(p); let t = \" */\";",
        // r69b: the walk is stricter than the text scan in one place —
        // a door METHOD is a door whatever the receiver, so the row the
        // text scan allowed as "some module `s`" is a door by `.open(..)`
        // (and `.read(..)`); the alias it would need is caught anyway.
        "let f = s::fs::OpenOptions::new().read(true).open(p);",
        // r69b: what an AST walk sees that no needle did — an alias
        // declared AFTER its use, a qualified type path, an `unsafe fn`,
        // an `unsafe impl`, a `std`-rooted glob deep in a group, a path
        // through a raw-identifier alias, and text that is not Rust at
        // all (rejected as such: it could not have compiled).
        "let t = s::fs::read_to_string(p); use std as s;",
        "let m: <Path>::Metadata = todo!(); let e = <Path>::exists(p);",
        "unsafe fn f() {}",
        "unsafe impl Send for X {}",
        "use std::{ffi, path::{Path, *}};",
        "use r#std as s; let t = s::fs::read_to_string(p);",
        "this is not rust",
        "let x = ;",
        // r70: doors inside a macro's token stream, and the real backend
        // constructed inside the kernel.
        "let n = format!(\"{}\", std::fs::read_to_string(p).unwrap());",
        "assert!(p.exists());",
        "debug_assert!(dir.canonicalize().is_ok());",
        "let m = matches!(std::fs::metadata(p), Ok(_));",
        "let v = vec![std::env::current_dir().unwrap()];",
        "panic!(\"{}\", std::env::var(\"HOME\").unwrap());",
        "let _ = write!(f, \"{:?}\", std::fs::read_dir(p));",
        "let fs = super::fs::StdFs; let e = fs.list_dir(dir);",
        "let e = StdFs.list_dir(dir);",
        "let e = StdFs::default().child(dir, name);",
        "let w = WasiFs::new(|_| Ok(Vec::new()));",
        "let o = OverlayFs::new(&disk, &buffers);",
    ];
    for text in smuggled {
        assert!(
            ambient_authority(text).is_some(),
            "smuggled past the ratchet: {text}"
        );
    }
    let allowed = [
        "use std::path::{Component, Path, PathBuf};",
        "use std::ffi::{OsStr, OsString};",
        "use std::collections::BTreeMap;",
        "use std::cell::OnceCell;",
        "use std::sync::Arc;",
        // Completed from the text scan's fragments (r69b): the walk
        // parses, so an item must be whole.
        "impl std::fmt::Display for RootError { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) } }",
        "impl std::error::Error for RootError {}",
        "let shared = ::std::sync::Arc::new(1);",
        "let other = mystd::thing::load(p);",
        "pub fn external(package: Arc<SchemaPackage>) -> Self { Self }",
        // The kernel's own vocabulary, which the door names must not
        // mistake for a door: the injected oracle's methods, `super`
        // globs, the separator constant.
        "use super::*;",
        "fn fold_absolute(path: &Path, fs: &dyn PathFs, stop: Option<&Path>) {}",
        "let (cur, names) = split_absolute(path).ok_or(Fold::NotAbsolute)?;",
        "let entries = fs.list_dir(dir)?;",
        "let step = fs.child(&cur, name)?; let t = open.resolve_symlink(&cur, name);",
        "let sep = std::path::MAIN_SEPARATOR;",
        "// `.` reaches here only from a Windows verbatim spelling",
        // r54 #2: a string, `as_ref`/`as_str`, a comment's `as`, an
        // associated item, and the kernel's `NotAbsolute`.
        "impl Trust { fn as_str(&self) -> &str { \"std\" } }",
        "let x = \"std as a string\";",
        "let s = std::sync::Arc::new(1); let n = s.as_ref();",
        "// core::fmt as the formatting root",
        "let assoc = Type::assoc;",
        "let e = Fold::NotAbsolute;",
        "let e = RootError::NotAbsolute;",
        // r56 #6: the alias scan's token boundary, both sides.
        "let x = alias as u8;",
        "let stdx = 1; let y = stdx as u8;",
        "let n = mod_std as usize;",
        "let (core, assoc) = (schema, Type::assoc);",
        "let v = v as usize; // std as a comment",
        "let t = \"use std as s;\";",
        "let t = r#\"use std as s;\"#;",
        "fn fold(cur: &Path, names: &[&OsStr]) -> Result<PathBuf, Fold> { Err(Fold::NotAbsolute) }",
        // r58 #2: a comment compiles to nothing; a `//` or `/*` inside a
        // string or a char literal is still text.
        "// std::fs::read_to_string is not reached here",
        "/* p.exists() would be a door */ let n = names.len();",
        "let sep = '/'; let url = \"http://x\"; let n = names.len();",
        "let sep = \"/*\"; let n = names.len(); // not a block comment",
        // r60: the escaped-quote char literal, alone and before a string.
        "let q = '\\''; let n = names.len();",
        "let _ = stringify!('\\''\"'\"); let n = names.len();",
        // r69b: a local named like a door crate is a local (the
        // prototype's one false positive), a `dirs` VALUE included; a
        // door name as a struct field or a variant is not a call.
        "let dirs: Vec<SourceKey> = Vec::new(); for d in &dirs { let _ = d; }",
        "let args = 1; let n = args + 1;",
        "struct S { metadata: u8 } let s = S { metadata: 1 }; let m = s.metadata;",
        "let e = Fold::Absent; let k = EntryKind::File;",
        "#[cfg(windows)] fn canonical() {}",
        "#[cfg_attr(test, derive(Debug))] struct T;",
        // r70: the kernel's own macro uses stay clean.
        "let locator = format!(\"declared source `{file}` (schemas[{index}].file in `{key}`)\");",
        "let v = vec![SourceKey::root()]; debug_assert!(v.len() == 1, \"{}\", v.len());",
        "impl fmt::Display for X { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, \"{}\", self.0) } }",
        "let k = matches!(kind, EntryKind::File | EntryKind::Dir);",
    ];
    for text in allowed {
        assert_eq!(ambient_authority(text), None, "{text}");
    }
}

/// r69b: the ratchet fires on the REAL kernel when one door is let
/// through — the allow-list is load-bearing, not decorative. (The
/// mutation "remove a door from the list" is RED on the row set above;
/// this pin is the positive half: a kernel file plus one door line is
/// caught by the same walk that passes the file.)
#[test]
fn source_ratchet_catches_one_door_added_to_a_kernel_file() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workspace");
    let text = std::fs::read_to_string(dir.join("paths.rs")).expect("paths.rs reads");
    assert_eq!(ambient_authority(&text), None);
    let doored = format!("{text}\nfn door(p: &Path) -> bool {{ p.exists() }}\n");
    assert_eq!(
        ambient_authority(&doored).as_deref(),
        Some("method call `.exists(..)` (a door by name)")
    );
}
