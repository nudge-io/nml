//! The workspace kernel's pins. Every test here runs over the scripted
//! [`MockFs`](super::mock::MockFs) — the real filesystem is exercised by
//! the crate's integration tests (`tests/workspace_fs.rs`), where
//! `CARGO_TARGET_TMPDIR` exists and the ambient-authority ratchet below
//! does not apply.

mod claims;
mod diag;
mod discover;
mod grants;
#[cfg(unix)]
mod mock;
mod paths;
mod ratchet;
mod vocabulary;

use std::path::Path;

use super::mock::MockFs;
use super::*;

/// The canonical mock root: a tree under `/ws`.
fn root_at(fs: &MockFs, dir: &str) -> WorkspaceRoot {
    WorkspaceRoot::explicit(Path::new(dir), fs).expect("root canonicalizes")
}

fn mint(fs: &MockFs, root: &WorkspaceRoot, path: &str, trust: Trust) -> Result<Keyed, PathError> {
    SourceKey::mint(root, Path::new(path), fs, trust)
}

// ───────────────────────────────────────────── a scripted workspace ──

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use nml_core::diagnostic::Diagnostic;

use crate::package::SchemaPackage;
use crate::test_support::DEMO_CORE;

/// A scripted workspace: the mock tree discovery walks plus the texts it
/// reads — one fixture vocabulary for the claims, discovery and grant
/// pins. Keys are root-relative; the root is `/ws`.
struct Ws {
    fs: MockFs,
    texts: HashMap<PathBuf, String>,
    /// Every path the reader was asked for, in order: the executable
    /// form of "an inert manifest is never read" (E28).
    reads: RefCell<Vec<PathBuf>>,
}

/// The manifest text for package `name`: one `core` schema beside it,
/// the given bindings (`(binding, globs)`) in order, optional markers.
fn manifest_text(name: &str, markers: &[&str], validators: &[(&str, &[&str])]) -> String {
    manifest_text_with_units(name, markers, &[], validators)
}

/// [`manifest_text`] with declared budget units (r88 P11 spike).
fn manifest_text_with_units(
    name: &str,
    markers: &[&str],
    units: &[&str],
    validators: &[(&str, &[&str])],
) -> String {
    let mut text = format!("package {name}:\n    version = \"0.1.0\"\n    formatVersion = 1\n");
    if !units.is_empty() {
        text.push_str("    budgetUnits:\n");
        for u in units {
            text.push_str(&format!("        - \"{u}\"\n"));
        }
    }
    if !markers.is_empty() {
        text.push_str("    rootMarkers:\n");
        for m in markers {
            text.push_str(&format!("        - \"{m}\"\n"));
        }
    }
    text.push_str("\n[]schema schemas:\n    - core:\n        file = \"core.model.nml\"\n\n[]validator validators:\n");
    for (binding, globs) in validators {
        text.push_str(&format!("    - {binding}:\n        files:\n"));
        for g in *globs {
            text.push_str(&format!("            - \"{g}\"\n"));
        }
        text.push_str("        schemas:\n            - core\n        strict = true\n");
    }
    text
}

impl Ws {
    fn new() -> Self {
        Self {
            fs: MockFs::new().dir("/ws"),
            texts: HashMap::new(),
            reads: RefCell::new(Vec::new()),
        }
    }

    fn abs(key: &str) -> PathBuf {
        PathBuf::from(format!("/ws/{key}"))
    }

    fn text(mut self, key: &str, text: &str) -> Self {
        self.fs = self.fs.file(&format!("/ws/{key}"));
        self.texts.insert(Self::abs(key), text.to_string());
        self
    }

    /// A content file (empty text).
    fn file(self, key: &str) -> Self {
        self.text(key, "")
    }

    /// A workspace manifest at `key` plus its `core.model.nml` beside it.
    fn manifest(
        self,
        at: &str,
        name: &str,
        markers: &[&str],
        validators: &[(&str, &[&str])],
    ) -> Self {
        let core = key(at).dir().join("core.model.nml");
        self.text(at, &manifest_text(name, markers, validators))
            .text(core.as_str(), DEMO_CORE)
    }

    fn config(self, key: &str, body: &str) -> Self {
        self.text(key, &format!("project p:\n{body}"))
    }

    fn symlink(mut self, key: &str, target: &str) -> Self {
        self.fs = self.fs.symlink(&format!("/ws/{key}"), target);
        self
    }

    fn denied(mut self, key: &str) -> Self {
        self.fs = self.fs.denied(&format!("/ws/{key}"));
        self
    }

    fn root(&self) -> WorkspaceRoot {
        WorkspaceRoot::explicit(Path::new("/ws"), &self.fs).unwrap()
    }

    fn read(&self) -> impl Fn(InputKind, &Path) -> Result<String, String> + '_ {
        move |_: InputKind, p: &Path| {
            self.reads.borrow_mut().push(p.to_path_buf());
            self.texts
                .get(p)
                .cloned()
                .ok_or_else(|| "absent".to_string())
        }
    }

    /// The keys the reader was asked for so far.
    fn read_keys(&self) -> Vec<String> {
        self.reads
            .borrow()
            .iter()
            .map(|p| {
                let spelled = p.to_string_lossy().replace('\\', "/");
                spelled
                    .strip_prefix("/ws/")
                    .or_else(|| spelled.strip_prefix("ws/"))
                    .expect("reads stay under the root")
                    .to_string()
            })
            .collect()
    }

    fn discover(&self, root: &WorkspaceRoot, extra: Vec<ExternalClaim>) -> Discovery {
        crate::workspace::discover::discover(root, &self.fs, &self.read(), extra, Arc::default())
    }

    /// [`Self::discover`] under scaled entry bounds (the default lane's
    /// backstop pins).
    fn discover_scaled(
        &self,
        root: &WorkspaceRoot,
        extra: Vec<ExternalClaim>,
        unit_bound: usize,
        total_bound: usize,
    ) -> Discovery {
        crate::workspace::discover::discover_scaled(
            root,
            &self.fs,
            &self.read(),
            extra,
            unit_bound,
            total_bound,
        )
    }

    /// [`Self::discover`] under scaled live-input BYTE bounds (the
    /// default lane's byte-backstop pin).
    fn discover_byte_scaled(
        &self,
        root: &WorkspaceRoot,
        extra: Vec<ExternalClaim>,
        unit_bytes: usize,
        total_bytes: usize,
    ) -> Discovery {
        crate::workspace::discover::discover_byte_scaled(
            root,
            &self.fs,
            &self.read(),
            extra,
            unit_bytes,
            total_bytes,
        )
    }

    /// A FIFO, socket or device at `key`.
    fn other(mut self, key: &str) -> Self {
        self.fs = self.fs.other(&format!("/ws/{key}"));
        self
    }
}

/// An external (store/injected/builtin) package built from a manifest
/// text, its `core` source supplied inline.
fn external(
    name: &str,
    markers: &[&str],
    validators: &[(&str, &[&str])],
    class: ExternalClass,
) -> ExternalClaim {
    let package = SchemaPackage::from_parts(&manifest_text(name, markers, validators), |_| {
        Ok(DEMO_CORE.to_string())
    })
    .expect("external package loads");
    ExternalClaim::new(Arc::new(package), class)
}

/// A key from its spelling — the test's way in; production mints.
fn key(s: &str) -> SourceKey {
    SourceKey::checked(s).unwrap_or_else(|| panic!("{s:?} is not a key spelling"))
}

/// The bound claimant's `(package, binding, glob index, anchor)`.
fn bound(g: &Governing<'_>) -> (String, String, usize, String) {
    match g {
        Governing::Bound { claimant, .. } => (
            claimant.claim.name().to_string(),
            claimant.binding.name.clone(),
            claimant.glob,
            claimant.anchor.as_str().to_string(),
        ),
        other => panic!("not bound: {other:?}"),
    }
}

fn manifest_keys(d: &Discovery) -> Vec<String> {
    d.claims
        .iter()
        .filter_map(|c| c.manifest().map(|k| k.as_str().to_string()))
        .collect()
}

fn inert_keys(d: &Discovery) -> Vec<String> {
    d.inert.iter().filter_map(|d| d.source.clone()).collect()
}

/// The walk's one enumeration, as spelled.
fn files(d: &Discovery) -> Vec<String> {
    d.files.iter().map(|k| k.as_str().to_string()).collect()
}

/// Diagnostics as they print (`Display` carries source, severity, code
/// and message) — the comparable form.
fn shown(ds: &[Diagnostic]) -> Vec<String> {
    ds.iter().map(ToString::to_string).collect()
}
