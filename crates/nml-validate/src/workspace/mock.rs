//! A scripted [`PathFs`] with a PROBE LOG (feature `test-support`): the
//! executable form of the kernel's no-existence-oracle rule. Tests and
//! the `paths` fuzz target script a tree — directories, files, symlink
//! edges, EACCES and ELOOP nodes, a lookup-insensitivity flag, the
//! spelling regime — and assert on what the kernel asked, not only on
//! what it answered.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::fs::{EntryKind, FsError, LstatFs, PathFs, Step};

/// The rooted spelling [`super::paths::split_absolute`] + a plain-name fold
/// yields — the same one [`WorkspaceRoot::explicit`] and the path kernel
/// use when they probe. Tests script POSIX `/ws/...` trees; on Windows
/// those spellings are not the `PathBuf` keys `components` hands back, so
/// the mock normalizes every scripted and oracle path through here.
fn script_path(path: &Path) -> PathBuf {
    let Some((mut cur, names)) = super::paths::split_absolute(path) else {
        return path.to_path_buf();
    };
    for name in names {
        match name.as_os_str() {
            name if name == "." => {}
            name if name == ".." => {
                cur.pop();
            }
            name => cur.push(name),
        }
    }
    cur
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    File,
    Dir,
    /// A directory whose entries cannot be looked up or listed (EACCES).
    Denied,
    /// A directory that is SEARCHABLE but not LISTABLE (mode `0111`, a
    /// hardening an operator chooses on purpose): a lookup under it
    /// answers, a listing of it is refused (EACCES). The shape `Denied`
    /// cannot script — its lookups refuse too — and the one the shadow
    /// check's fence rule is stated over.
    Unlistable,
    /// Target as written (may be relative, may contain `..`).
    Symlink(String),
    /// A FIFO, socket or device: exists, is neither file nor directory.
    Other,
}

/// How the mock proves a spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spelling {
    /// Like a native realpath: the on-disk entry name, verified.
    Realpath,
    /// Like the wasi backend: verified iff the lookup name is a byte-exact
    /// listing member; symlinks cannot be resolved (`NoRealpath`).
    Membership,
}

/// One oracle call, as the kernel made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Child(PathBuf, OsString),
    ListDir(PathBuf),
    ResolveSymlink(PathBuf, OsString),
}

impl Probe {
    /// The entry NAME this probe named, if any (listings name none).
    pub fn name(&self) -> Option<&OsStr> {
        match self {
            Self::Child(_, n) | Self::ResolveSymlink(_, n) => Some(n),
            Self::ListDir(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct MockFs {
    nodes: BTreeMap<PathBuf, Node>,
    /// Case-insensitive lookup (APFS/NTFS): `admin` finds `Admin`.
    lookup_insensitive: bool,
    spelling: Spelling,
    log: RefCell<Vec<Probe>>,
}

impl Default for MockFs {
    fn default() -> Self {
        Self::new()
    }
}

impl MockFs {
    /// An empty tree holding only `/`.
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(script_path(Path::new("/")), Node::Dir);
        Self {
            nodes,
            lookup_insensitive: false,
            spelling: Spelling::Realpath,
            log: RefCell::new(Vec::new()),
        }
    }

    pub fn insensitive(mut self) -> Self {
        self.lookup_insensitive = true;
        self
    }

    pub fn spelling(mut self, spelling: Spelling) -> Self {
        self.spelling = spelling;
        self
    }

    /// Any path — a `&str`, or on unix an `OsStr` built from bytes, so a
    /// scripted tree can hold a name that is not UTF-8 (the walk reports
    /// it; the fuzz invariant checks that it does).
    fn insert(mut self, path: &(impl AsRef<Path> + ?Sized), node: Node) -> Self {
        let path = script_path(path.as_ref());
        // Every ancestor exists as a directory unless scripted otherwise.
        let mut ancestors: Vec<PathBuf> = path.ancestors().skip(1).map(Path::to_path_buf).collect();
        ancestors.reverse();
        for a in ancestors {
            self.nodes.entry(a).or_insert(Node::Dir);
        }
        self.nodes.insert(path, node);
        self
    }

    pub fn dir(self, path: &(impl AsRef<Path> + ?Sized)) -> Self {
        self.insert(path, Node::Dir)
    }

    pub fn file(self, path: &(impl AsRef<Path> + ?Sized)) -> Self {
        self.insert(path, Node::File)
    }

    pub fn symlink(self, path: &(impl AsRef<Path> + ?Sized), target: &str) -> Self {
        self.insert(path, Node::Symlink(target.to_string()))
    }

    /// A FIFO, socket or device at `path`.
    pub fn other(self, path: &(impl AsRef<Path> + ?Sized)) -> Self {
        self.insert(path, Node::Other)
    }

    pub fn denied(self, path: &(impl AsRef<Path> + ?Sized)) -> Self {
        self.insert(path, Node::Denied)
    }

    /// A directory that answers lookups but refuses to be listed (mode
    /// `0111`).
    pub fn unlistable(self, path: &(impl AsRef<Path> + ?Sized)) -> Self {
        self.insert(path, Node::Unlistable)
    }

    /// The probes made so far, in order.
    pub fn probes(&self) -> Vec<Probe> {
        self.log.borrow().clone()
    }

    pub fn clear_probes(&self) {
        self.log.borrow_mut().clear();
    }

    /// Whether any probe so far NAMED `name` (as a `child` or a symlink
    /// resolution) — the leaf-avoidance assertion.
    pub fn named(&self, name: &str) -> bool {
        self.log
            .borrow()
            .iter()
            .any(|p| p.name().is_some_and(|n| n == name))
    }

    /// The scripted kind of the entry at an EXACT `path` — a test oracle
    /// over the node table (no probe is recorded): the fuzz target and
    /// the `..`-resume pins check that no existing component of a key
    /// minted under closed trust is a symlink.
    pub fn kind_at(&self, path: &Path) -> Option<EntryKind> {
        self.nodes.get(&script_path(path)).map(|node| match node {
            Node::File => EntryKind::File,
            Node::Other => EntryKind::Other,
            Node::Dir | Node::Denied | Node::Unlistable => EntryKind::Dir,
            Node::Symlink(_) => EntryKind::Symlink,
        })
    }

    fn record(&self, probe: Probe) {
        self.log.borrow_mut().push(probe);
    }

    /// The on-disk entry under `dir` matching `name` under the lookup
    /// regime: `(on-disk name, node)`.
    fn lookup(&self, dir: &Path, name: &OsStr) -> Option<(OsString, Node)> {
        let want = name.to_str()?;
        let exact = dir.join(name);
        if let Some(node) = self.nodes.get(&exact) {
            return Some((name.to_os_string(), node.clone()));
        }
        if !self.lookup_insensitive {
            return None;
        }
        self.nodes.iter().find_map(|(p, node)| {
            let n = p.file_name()?.to_str()?;
            (p.parent() == Some(dir) && n.eq_ignore_ascii_case(want))
                .then(|| (OsString::from(n), node.clone()))
        })
    }

    /// Resolve a symlink target the way realpath would: relative to the
    /// link's directory, lexical `..`, further links followed, a hop
    /// budget standing in for ELOOP.
    fn resolve(&self, dir: &Path, target: &str, hops: usize) -> Result<Option<PathBuf>, FsError> {
        if hops > 40 {
            return Err(FsError::SymlinkLoop);
        }
        let joined = if target.starts_with('/') {
            script_path(Path::new(target))
        } else {
            dir.join(target)
        };
        let mut cur = script_path(Path::new("/"));
        let comps: Vec<OsString> = joined
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_os_string()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .collect();
        for name in comps {
            if name == ".." {
                cur.pop();
                continue;
            }
            match self.lookup(&cur, &name) {
                None => return Ok(None),
                Some((_, Node::Symlink(t))) => match self.resolve(&cur, &t, hops + 1)? {
                    Some(p) => cur = p,
                    None => return Ok(None),
                },
                Some((_, Node::Denied)) => return Err(FsError::Denied),
                Some((spelled, _)) => cur.push(spelled),
            }
        }
        Ok(Some(cur))
    }
}

impl LstatFs for MockFs {
    fn child(&self, dir: &Path, name: &OsStr) -> Result<Option<Step>, FsError> {
        let dir = script_path(dir);
        self.record(Probe::Child(dir.clone(), name.to_os_string()));
        match self.nodes.get(&dir) {
            Some(Node::Denied) => return Err(FsError::Denied),
            Some(Node::Dir) | Some(Node::Unlistable) => {}
            _ => return Ok(None),
        }
        let Some((spelled, node)) = self.lookup(&dir, name) else {
            return Ok(None);
        };
        let kind = match node {
            Node::File => EntryKind::File,
            Node::Other => EntryKind::Other,
            Node::Dir | Node::Denied | Node::Unlistable => EntryKind::Dir,
            Node::Symlink(_) => EntryKind::Symlink,
        };
        if kind == EntryKind::Symlink {
            return Ok(Some(Step {
                spelling: name.to_os_string(),
                kind,
                spelling_verified: false,
            }));
        }
        Ok(Some(match self.spelling {
            Spelling::Realpath => Step {
                spelling: spelled,
                kind,
                spelling_verified: true,
            },
            Spelling::Membership => Step {
                spelling: name.to_os_string(),
                kind,
                spelling_verified: spelled == name,
            },
        }))
    }

    fn list_dir(&self, dir: &Path) -> Result<Vec<(OsString, EntryKind)>, FsError> {
        let dir = script_path(dir);
        self.record(Probe::ListDir(dir.clone()));
        match self.nodes.get(&dir) {
            Some(Node::Denied) | Some(Node::Unlistable) => return Err(FsError::Denied),
            Some(Node::Dir) => {}
            _ => return Err(FsError::Io(None)),
        }
        Ok(self
            .nodes
            .iter()
            .filter(|(p, _)| p.parent() == Some(dir.as_path()))
            .filter_map(|(p, node)| {
                let kind = match node {
                    Node::File => EntryKind::File,
                    Node::Other => EntryKind::Other,
                    Node::Dir | Node::Denied | Node::Unlistable => EntryKind::Dir,
                    Node::Symlink(_) => EntryKind::Symlink,
                };
                Some((p.file_name()?.to_os_string(), kind))
            })
            .collect())
    }
}

impl PathFs for MockFs {
    fn resolve_symlink(&self, dir: &Path, name: &OsStr) -> Result<Option<PathBuf>, FsError> {
        let dir = script_path(dir);
        self.record(Probe::ResolveSymlink(dir.clone(), name.to_os_string()));
        if self.spelling == Spelling::Membership {
            return Err(FsError::NoRealpath);
        }
        match self.lookup(&dir, name) {
            Some((_, Node::Symlink(target))) => self.resolve(&dir, &target, 0),
            Some(_) => Ok(Some(dir.join(name))),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The [`LstatFs::list_dir`] contract at this boundary: the mock
    /// lists SORTED by construction (a `BTreeMap` over the paths), in
    /// whatever order the tree was built — so an ordering claim proven
    /// over the mock is a claim about the kernel's reliance on the
    /// contract, never about the mock's insertion order.
    /// The scripted `0111` directory: a child under it is found (and a
    /// descendant's listing answers), its own listing is refused, and
    /// refused as `Denied` — the real oracle's word for EACCES.
    #[test]
    fn an_unlistable_directory_answers_lookups_and_refuses_listings() {
        let fs = MockFs::new()
            .unlistable("/srv")
            .file("/srv/app/x.nml")
            .file("/srv/evil.package.nml");
        let app = fs
            .child(Path::new("/srv"), OsStr::new("app"))
            .unwrap()
            .unwrap();
        assert_eq!((app.kind, app.spelling_verified), (EntryKind::Dir, true));
        assert_eq!(fs.list_dir(Path::new("/srv")).unwrap_err(), FsError::Denied);
        assert_eq!(
            fs.list_dir(Path::new("/srv/app")).unwrap(),
            vec![(OsString::from("x.nml"), EntryKind::File)]
        );
        assert_eq!(fs.kind_at(Path::new("/srv")), Some(EntryKind::Dir));
    }

    #[test]
    fn listings_are_sorted_by_construction() {
        let fs = MockFs::new()
            .dir("/ws")
            .file("/ws/b.nml")
            .dir("/ws/c")
            .file("/ws/a");
        assert_eq!(
            fs.list_dir(Path::new("/ws")).unwrap(),
            vec![
                (OsString::from("a"), EntryKind::File),
                (OsString::from("b.nml"), EntryKind::File),
                (OsString::from("c"), EntryKind::Dir),
            ]
        );
    }
}
