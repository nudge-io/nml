//! The per-file instance index: discovery of composable instances and `uses` ref resolution.

use std::collections::HashMap;

use crate::ast::{BlockDecl, File};
use crate::query::Document;

/// Identity of a composed instance: defining file + declaration name.
/// Names are file-scoped (RFC 0020), so diamonds dedupe by this pair,
/// never by name alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstanceId<'a> {
    /// Canonical workspace-relative defining path.
    pub source_path: &'a str,
    pub name: &'a str,
}

/// Resolves an [`InstanceId`] to its declaring block and defining document.
/// This slice indexes one file (same-file composition — RFC 0020 imports
/// extend it across files); the API is already id-keyed so the extension is
/// additive.
pub struct InstanceIndex<'a> {
    source_path: &'a str,
    file: &'a File,
    by_name: HashMap<&'a str, &'a BlockDecl>,
}

impl<'a> InstanceIndex<'a> {
    pub fn from_file(source_path: &'a str, file: &'a File) -> Self {
        let mut by_name: HashMap<&str, &BlockDecl> = HashMap::new();
        for decl in &file.declarations {
            if let crate::ast::DeclarationKind::Block(b) = &decl.kind {
                if !crate::symbols::is_schema_keyword(&b.keyword.name) {
                    // First-wins on duplicate names — the documented
                    // convention everywhere (SchemaIndex, PolicyCtx), and
                    // the duplicate itself is NML2009's business. A plain
                    // `insert` would silently make the LAST duplicate the
                    // one every ref composes against.
                    by_name.entry(b.name.name.as_str()).or_insert(b);
                }
            }
        }
        Self {
            source_path,
            file,
            by_name,
        }
    }

    pub fn get(&self, id: InstanceId<'_>) -> Option<&'a BlockDecl> {
        (id.source_path == self.source_path)
            .then(|| self.by_name.get(id.name).copied())
            .flatten()
    }

    /// Resolve a bare `uses` ref through this file's scope (same-file names;
    /// RFC 0020 import bindings join here).
    pub fn resolve_ref(&self, name: &str) -> Option<InstanceId<'a>> {
        self.by_name.get(name).map(|b| InstanceId {
            source_path: self.source_path,
            name: &b.name.name,
        })
    }

    /// In-scope instance names (did-you-mean candidates for NML2059 and
    /// NML2062), in DECLARATION order.
    ///
    /// The order is the contract, not an accident of iteration: the
    /// suggester breaks distance ties toward the EARLIEST candidate
    /// ([`crate::suggest`]), so a vocabulary handed over in hash order
    /// makes the hint a different name on each run of the same binary
    /// over the same file — the composition golden records the hint, the
    /// `--json` wire carries it, and the editor and `nml check` are
    /// supposed to say the same thing about the same buffer. Walking the
    /// declarations and keeping the one [`Self::from_file`] indexed —
    /// first-wins on a duplicate name, the block every ref composes
    /// against — yields exactly the map's own names, in the order they
    /// were written.
    pub fn names(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.file.declarations.iter().filter_map(|decl| {
            let crate::ast::DeclarationKind::Block(b) = &decl.kind else {
                return None;
            };
            let name = b.name.name.as_str();
            let indexed = self.by_name.get(name).copied()?;
            std::ptr::eq(indexed, b).then_some(name)
        })
    }

    /// The layer's own document — RFC 0013 array refs are file-local, so
    /// step-3 inlining goes through this, never the composing file's.
    pub fn document(&self) -> Document<'a> {
        Document::new(self.file)
    }
}

// ─────────────────────────────────────────────────────── merge policies ──
