//! The workspace root and the canonical source key (RFC 0019 item 0,
//! step 0b): the path pipeline P1–P4, built over the [`PathFs`] oracle so
//! it has no ambient authority.
//!
//! Order is the whole point (E26). A key is minted by folding
//! [`LstatFs::child`] over the PARENT components of a path from the
//! canonical root — `lstat` before any resolution — so in a closed
//! universe a symlink component (ancestor or leaf, dangling or not) halts
//! the walk before its target is ever resolved: P4 precedes P3, and the
//! resulting NML2083 is byte-identical whether or not the target exists.
//! The leaf is never held at an oracle call during minting (structural
//! leaf-avoidance): existence is probed only by [`Keyed::verify`], which
//! the caller runs AFTER a grant allowed the key — so a disallowed path
//! diagnoses identically whether or not the file exists.
//!
//! The same order governs the DERIVED root (E28): the walk down to the
//! invocation target follows symlinks only while no `.git` fence has been
//! seen — the operator's `/tmp → /private/tmp` — and below the fence it
//! is `lstat`-only, stopping before the first symlink component; the
//! target's own leaf is never resolved by anyone but the trust-aware
//! walk. A derived root therefore never depends on where an author's
//! link points, nor on whether it points anywhere at all.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::file_names::{PROJECT_CONFIG_NAME, is_manifest_name};
use crate::fs::{EntryKind, FsError, LstatFs, PathFs};

/// Bound on key components — the parser's `MAX_DEPTH` / the glob
/// matcher's `MAX_PATTERN_SEGMENTS` posture: a bound on work, and the
/// E21 bound on the root walk. It bounds the AUTHORED component count
/// too (E28): a path is refused before its first probe, not after its
/// ten-thousandth.
///
/// LIMIT: reach=content guards=domain surface=kernel shown="64" — path components in a source key, a root walk or an authored path
pub const MAX_COMPONENTS: usize = 64;

/// How the workspace root was fixed for this invocation (R1): `--root` >
/// the editor's workspace folder > derived from the invocation target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RootOrigin {
    Explicit,
    /// The editor's workspace folder (R1's second rung).
    Editor,
    Derived {
        fence: Fence,
        /// What the shadow check found ABOVE the fence within the walk
        /// bound, nearest first: another `.git` entry (the fence is then
        /// a submodule's, a linked worktree's or a planted entry inside
        /// an outer repository), or a root marker above a DIRECTORY
        /// fence (a checkout nested under a workspace that would claim
        /// it, a stray manifest in a parent directory — E21's fence
        /// keeps it out of the universe, and a front end says so). A
        /// root marker above a fence that is no directory never reaches
        /// here — derivation refuses ([`RootError::Shadowed`]); a walk
        /// that could not finish the check refuses too
        /// ([`RootError::ShadowUnchecked`]).
        shadowed: Option<Shadow>,
    },
}

/// The entry the shadow check found above a derived root's fence. Its
/// path is the one fact a front end states (`root.shadowed`): the entry
/// names its own kind — `.git`, or a manifest / `nml-project.nml`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Shadow {
    /// A `.git` entry (of any kind) at this path: the outer
    /// repository's fence.
    Git(PathBuf),
    /// A workspace manifest or project config at this path, above a
    /// DIRECTORY fence: under `--root` at its directory it would anchor
    /// the universe this tree sits in.
    Marker(PathBuf),
}

impl Shadow {
    pub fn path(&self) -> &Path {
        match self {
            Self::Git(p) | Self::Marker(p) => p,
        }
    }
}

/// What bounded a derived root's upward walk (E21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fence {
    /// The nearest ancestor holding a `.git` entry of ANY kind — the
    /// entry's kind kept: a `.git` FILE is a linked worktree's, a
    /// submodule's or a planted one, and a front end discloses a fence
    /// that is not a directory.
    Vcs { kind: EntryKind },
    /// No VCS root found: the invocation target's own directory.
    TargetDir,
}

impl Fence {
    /// The fence entry's kind as the wire spells it (`dir`, `file`,
    /// `symlink`, `other`); `None` without a VCS fence.
    pub fn entry_tag(self) -> Option<&'static str> {
        match self {
            Self::Vcs { kind } => Some(match kind {
                EntryKind::Dir => "dir",
                EntryKind::File => "file",
                EntryKind::Symlink => "symlink",
                EntryKind::Other => "other",
            }),
            Self::TargetDir => None,
        }
    }
}

impl RootOrigin {
    /// The uniform origin tag: the FACT of where the universe came from,
    /// as one enum value of the `--json` vocabulary (`explicit`,
    /// `editor`, `derivedVcsFence`, `derivedTargetDir` — the `summary`
    /// and `binding` rows' `root.origin`, lowerCamel like every value in
    /// the stream). Layer A speaks no front end's advice (E35): "pass
    /// `--root` to pin" is the CLI's sentence, composed by the reporter
    /// that owns the flag — the editor has no `--root` to offer.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Editor => "editor",
            Self::Derived {
                fence: Fence::Vcs { .. },
                ..
            } => "derivedVcsFence",
            Self::Derived {
                fence: Fence::TargetDir,
                ..
            } => "derivedTargetDir",
        }
    }

    /// The facts an operator reading a DERIVED verdict must see, as one
    /// sentence with no front end's advice in it (E35): the fence
    /// entry's kind — a `.git` FILE is a linked worktree's, a
    /// submodule's or a planted one — and the shadow above it, when
    /// any: `within a .git FILE fence — a linked worktree's, a
    /// submodule's or a planted entry; SHADOWED by the root marker
    /// `…` above it`. `None` for an explicit or editor root and for a
    /// target-directory fence (no VCS root: the tag says it). Both
    /// front ends print it — the CLI after the root, then `--root` to
    /// pin; the editor in the derivation's log line, then the folder to
    /// open — so a worktree's or a planted `.git` file is disclosed in
    /// one sentence wherever a universe was derived within it. The
    /// shadow's path is spelled by `spell` — the front end's own rule,
    /// the one it spells the root with on the same line (the CLI from
    /// the working directory, the editor canonically): one sentence,
    /// each front end's spelling.
    pub fn fence_facts(&self, spell: &dyn Fn(&Path) -> String) -> Option<String> {
        let Self::Derived {
            fence: Fence::Vcs { kind },
            shadowed,
        } = self
        else {
            return None;
        };
        let entry = match kind {
            EntryKind::Dir => "the .git fence",
            EntryKind::File => {
                "a .git FILE fence — a linked worktree's, a submodule's or a planted entry"
            }
            EntryKind::Symlink => "a .git SYMLINK fence — a planted entry",
            EntryKind::Other => "a .git special-entry fence — a planted entry",
        };
        let shadow = match shadowed {
            Some(Shadow::Git(entry)) => format!(
                "; SHADOWED by another .git entry at `{}` above it",
                spell(entry)
            ),
            Some(Shadow::Marker(marker)) => {
                format!("; SHADOWED by the root marker `{}` above it", spell(marker))
            }
            None => String::new(),
        };
        Some(format!("within {entry}{shadow}"))
    }

    /// Whether a derivation is one an operator could not infer from the
    /// root alone, so a front end states it (the CLI's `note: workspace
    /// root …`, the editor's warning): a fence entry that is no
    /// directory, a shadow above the fence ([`Self::fence_facts`] says
    /// which), or NO fence at all — the universe is then the target's
    /// own directory, and a manifest above it is never seen; exactly the
    /// non-obvious root the disclosure exists for. A plain derivation (a
    /// `.git` directory fence with nothing above it) needs none.
    pub fn needs_disclosure(&self) -> bool {
        match self {
            Self::Derived {
                fence: Fence::TargetDir,
                ..
            } => true,
            Self::Derived {
                fence: Fence::Vcs { kind },
                shadowed,
            } => *kind != EntryKind::Dir || shadowed.is_some(),
            Self::Explicit | Self::Editor => false,
        }
    }
}

/// Why a root could not be fixed. CLI errors, not diagnostics: a root is
/// an operator input, not content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootError {
    /// The path is absent or not a directory — a `--root` that is a
    /// symlink to a FILE included: every resolved endpoint is kind-checked.
    NotADirectory,
    /// A relative root/target was given; the kernel has no working
    /// directory (no ambient authority) — the caller absolutizes.
    NotAbsolute,
    /// E21 with no VCS root: the invocation target's own directory — the
    /// fence itself — is a symlink, and a universe is never derived
    /// through a link. The target was not resolved, so this message is
    /// identical whether or not it exists; `--root` names the universe.
    UnfencedSymlink {
        component: String,
    },
    /// E21's work bound reached with no VCS root within it: the walk
    /// derives NOTHING — closed-denied — never the smaller OPEN universe
    /// the target's depth would have chosen (a file 70 directories
    /// below the operator's manifest validated under no binding at
    /// all). `last` is the last directory the walk visited.
    ComponentCap {
        last: PathBuf,
    },
    /// A root marker sits ABOVE a `.git` fence that is NO directory —
    /// a submodule's or linked worktree's `.git` file (the one entry
    /// git writes below another repository's manifest), a planted link
    /// or special entry — within the walk bound, with no other `.git`
    /// between: such an entry must not shrink the marker's universe
    /// silently. The caller names the root. (Above a DIRECTORY fence
    /// the marker is disclosed instead: [`Shadow::Marker`].)
    Shadowed {
        marker: PathBuf,
        fence: PathBuf,
    },
    /// The walk bound was reached while checking for a shadow ABOVE the
    /// fence: whether a marker or another `.git` sits there is unknown,
    /// and a universe is denied rather than derived unchecked — the
    /// bound is a work bound, never a fence. `last` is the last
    /// directory the check visited.
    ShadowUnchecked {
        fence: PathBuf,
        last: PathBuf,
    },
    Fs(FsError),
}

impl RootError {
    /// The sentence with every path spelled by `spell` — a front end that
    /// spells paths from its working directory (the CLI's `display_path`)
    /// passes its own speller, so a refusal reads like the lines around
    /// it; the `Display` form spells them canonically (the editor's).
    pub fn display_with(&self, spell: &dyn Fn(&Path) -> String) -> String {
        match self {
            Self::NotADirectory => "not a directory".to_string(),
            Self::NotAbsolute => "not an absolute path".to_string(),
            Self::UnfencedSymlink { component } => format!(
                "the target's directory `{component}` is a symlink and no VCS root fences \
                 the walk — a universe is never derived through a link"
            ),
            Self::ComponentCap { last } => format!(
                "no VCS root within {MAX_COMPONENTS} directories above `{}` — the walk bound \
                 was reached, and a universe is denied rather than narrowed to the target's \
                 own directory",
                spell(last)
            ),
            Self::Shadowed { marker, fence } => format!(
                "the root marker `{}` sits above the fence at `{}`, and that fence is no \
                 directory — a submodule's or linked worktree's .git file, or a planted entry, \
                 below a workspace manifest cannot shrink its universe",
                spell(marker),
                spell(fence)
            ),
            Self::ShadowUnchecked { fence, last } => format!(
                "the shadow check above the fence at `{}` reached the walk bound of \
                 {MAX_COMPONENTS} directories at `{}` — a universe is denied rather than \
                 derived unchecked",
                spell(fence),
                spell(last)
            ),
            Self::Fs(e) => e.to_string(),
        }
    }
}

impl std::fmt::Display for RootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_with(&|p: &Path| p.display().to_string().replace('\\', "/")))
    }
}

impl std::error::Error for RootError {}

impl From<FsError> for RootError {
    fn from(e: FsError) -> Self {
        Self::Fs(e)
    }
}

/// The once-per-invocation workspace root: the universe identity every
/// key is relative to and every grant glob is anchored under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRoot {
    canonical: PathBuf,
    pub(crate) origin: RootOrigin,
}

impl WorkspaceRoot {
    /// `--root <dir>`: the operator's universe, canonicalized (symlinks in
    /// an operator-given path are followed — it is not author-writable;
    /// the endpoint must be a directory).
    pub fn explicit(dir: &Path, fs: &dyn PathFs) -> Result<Self, RootError> {
        Ok(Self::at(canonical_dir(dir, fs)?, RootOrigin::Explicit))
    }

    /// The editor's workspace folder containing the target.
    pub fn editor(dir: &Path, fs: &dyn PathFs) -> Result<Self, RootError> {
        Ok(Self::at(canonical_dir(dir, fs)?, RootOrigin::Editor))
    }

    /// E21: from the invocation target's directory (`target_dir` — the
    /// fence-aware walk that never resolves an author's link), the fence
    /// is the nearest ancestor holding a `.git` entry of any kind; with
    /// no VCS root, the target's own directory. Within the fence the
    /// OUTERMOST directory holding a root marker is the root, else the
    /// target's directory. At most [`MAX_COMPONENTS`] directories are
    /// visited, and a walk that meets no fence within them derives
    /// nothing ([`RootError::ComponentCap`]): the bound is a work bound,
    /// never a fence that narrows the universe to an open one. Then the
    /// shadow check (`shadow_above`): a root marker above a fence that
    /// is no directory refuses, a marker above a directory fence and
    /// another `.git` above either are reported, and a check the bound
    /// cut short refuses.
    pub fn derive(target: &Path, fs: &dyn PathFs) -> Result<Self, RootError> {
        let target_dir = target_dir(target, fs)?;
        let mut chain: Vec<PathBuf> = vec![target_dir];
        let mut fence = None;
        loop {
            let dir = chain.last().expect("chain starts non-empty");
            // Every ancestor here was probed as a `child` by
            // `target_dir`'s fold, so a `.git` probe cannot be denied
            // where the fold was not: an `Err` is an I/O failure, never
            // a second "outside the universe" verdict (the arm that read
            // `Denied` as one was unreachable).
            if let Some(step) = fs.child(dir, OsStr::new(".git")).map_err(RootError::Fs)? {
                fence = Some((Fence::Vcs { kind: step.kind }, dir.clone()));
                break;
            }
            if chain.len() >= MAX_COMPONENTS {
                return Err(RootError::ComponentCap { last: dir.clone() });
            }
            match dir.parent() {
                Some(parent) => chain.push(parent.to_path_buf()),
                None => break,
            }
        }
        let (fence, shadowed) = match fence {
            Some((fence, fence_dir)) => {
                let dir_fence = matches!(
                    fence,
                    Fence::Vcs {
                        kind: EntryKind::Dir
                    }
                );
                (fence, shadow_above(&fence_dir, chain.len(), dir_fence, fs)?)
            }
            // No VCS root: the target's own directory is the whole fence.
            None => {
                chain.truncate(1);
                (Fence::TargetDir, None)
            }
        };
        // Outermost first: the first directory holding a manifest or a
        // project config wins. A listing the oracle refuses is not "no
        // marker" — it is an unknown answer, and a universe is never
        // derived on one.
        let mut marked = None;
        for dir in chain.iter().rev() {
            if root_marker_in(dir, fs)?.is_some() {
                marked = Some(dir.clone());
                break;
            }
        }
        let root = marked.unwrap_or_else(|| chain[0].clone());
        Ok(Self::at(root, RootOrigin::Derived { fence, shadowed }))
    }

    fn at(canonical: PathBuf, origin: RootOrigin) -> Self {
        Self { canonical, origin }
    }

    pub fn path(&self) -> &Path {
        &self.canonical
    }

    /// HOW this root was fixed — the run's own fact, stated once on
    /// every `binding`/`summary` row and in the editor's log line.
    /// Read-only: a root's origin is settled where the root is built
    /// (`explicit`, `editor`, `derive`), and a caller that could
    /// reassign it would make the row lie about the universe it names.
    pub fn origin(&self) -> &RootOrigin {
        &self.origin
    }

    /// The filesystem path a key names — the ONE way a renderer or reader
    /// turns a key back into a path (L1: note locators go through here).
    pub fn path_of(&self, key: &SourceKey) -> PathBuf {
        let mut path = self.canonical.clone();
        for component in key.components() {
            path.push(component);
        }
        path
    }
}

/// The `*.package.nml` or `nml-project.nml` `dir` holds as a regular
/// FILE, if any (the first in listing order) — discovery never loads a
/// link or a device, so neither anchors a universe.
///
/// A listing the oracle REFUSES is not "no marker": the answer is
/// unknown, and it is the CALLER that decides what the unknown costs.
/// Over the chain from the target's directory to the fence — the
/// directories that could BE the root — it denies the derivation rather
/// than derive on it (the rule [`shadow_above`] already applies to its
/// own bound, and the one `collect_listing` applies to a single
/// unreadable entry); ABOVE a directory fence, where no marker could
/// deny anything, [`shadow_above`] reads it as the lost disclosure it
/// is and walks on. It
/// used to be swallowed (`list_dir(dir).ok()`), so a directory that is
/// searchable but not listable — mode `0111`, a hardening an operator
/// chooses on purpose — silently held no manifest: the universe shrank
/// to the target's own directory, every binding and every `strict` with
/// it, and the run went green.
fn root_marker_in(dir: &Path, fs: &dyn LstatFs) -> Result<Option<PathBuf>, FsError> {
    Ok(fs.list_dir(dir)?.iter().find_map(|(name, kind)| {
        (*kind == EntryKind::File
            && name
                .to_str()
                .is_some_and(|n| is_manifest_name(n) || n == PROJECT_CONFIG_NAME))
        .then(|| dir.join(name))
    }))
}

/// The shadow check: a `.git` entry BELOW a workspace manifest — a
/// submodule's `.git` file, a planted entry — must not shrink the
/// operator's universe silently. From `fence_dir` upward, within the
/// same [`MAX_COMPONENTS`] bound the root walk spent `visited` of: a
/// root marker above a fence that is NO directory REFUSES derivation
/// (that entry is what git writes for a submodule or a linked worktree,
/// or what a tenant plants — the operator names the root); above a
/// DIRECTORY fence (`dir_fence`: a nested checkout, a stray marker in a
/// parent — E21's fence keeps it out of the universe) it is reported
/// as the shadow; another `.git` above, with no marker between, is the
/// outer repository's fence and is reported likewise. The walk stops at
/// the first of either. Reaching the bound with neither seen REFUSES
/// ([`RootError::ShadowUnchecked`]): the answer is unknown, and a
/// universe is never derived on an unfinished check — the bound is a
/// work bound, never a fence.
///
/// An ancestor the oracle cannot LIST is an unknown marker, and what
/// that unknown COSTS is the fence's: above a `.git` entry that is no
/// directory an unseen marker is the [`RootError::Shadowed`] refusal, so
/// a blinded listing refuses too ([`RootError::Fs`]) — fail-closed on
/// the one shape this check can deny. Above a DIRECTORY fence NEITHER
/// outcome of this listing can deny: a marker there is the disclosed
/// [`Shadow::Marker`] and a `.git` above is [`Shadow::Git`], so a
/// blinded listing costs a DISCLOSURE, never a universe, and the walk
/// goes on. Refusing there instead put every ordinary run behind the
/// mode of every directory between the fence and `/` — a parent an
/// operator hardened to `0111`, a privacy-gated folder, another
/// account's home — and denied a universe to print a sentence it could
/// not compute. (The `.git` probe is a LOOKUP, which `0111` answers, so
/// [`Shadow::Git`] is found either way; every ancestor was already
/// probed as a `child` by `target_dir`'s fold, so that probe fails only
/// on I/O.)
fn shadow_above(
    fence_dir: &Path,
    visited: usize,
    dir_fence: bool,
    fs: &dyn PathFs,
) -> Result<Option<Shadow>, RootError> {
    let mut visited = visited;
    let mut last = fence_dir.to_path_buf();
    let mut dir = fence_dir.parent();
    while let Some(d) = dir {
        if visited >= MAX_COMPONENTS {
            return Err(RootError::ShadowUnchecked {
                fence: fence_dir.join(".git"),
                last,
            });
        }
        visited += 1;
        // Blinded above a directory fence is a lost disclosure, not a
        // lost universe (see above); blinded above any other fence is
        // the `Shadowed` refusal it could not rule out.
        let marker = match root_marker_in(d, fs) {
            Ok(marker) => marker,
            Err(_) if dir_fence => None,
            Err(e) => return Err(RootError::Fs(e)),
        };
        if let Some(marker) = marker {
            return if dir_fence {
                Ok(Some(Shadow::Marker(marker)))
            } else {
                Err(RootError::Shadowed {
                    marker,
                    fence: fence_dir.join(".git"),
                })
            };
        }
        if fs
            .child(d, OsStr::new(".git"))
            .map_err(RootError::Fs)?
            .is_some()
        {
            return Ok(Some(Shadow::Git(d.join(".git"))));
        }
        last = d.to_path_buf();
        dir = d.parent();
    }
    Ok(None)
}

/// Whether `dir` or any of its ancestors holds a `.git` entry of any kind
/// (bounded like the root walk). `dir` is canonical, so its ancestors are
/// real directories.
fn vcs_fenced(dir: &Path, fs: &dyn LstatFs) -> Result<bool, FsError> {
    for ancestor in dir.ancestors().take(MAX_COMPONENTS) {
        if fs.child(ancestor, OsStr::new(".git"))?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The kind of the entry at a canonical `path`, by one `child` probe of
/// its parent; the filesystem root is a directory by definition.
fn kind_at(fs: &dyn LstatFs, path: &Path) -> Result<Option<EntryKind>, FsError> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok(fs.child(parent, name)?.map(|step| step.kind)),
        _ => Ok(Some(EntryKind::Dir)),
    }
}

/// An absolute path's prefix (`/`, or a drive prefix plus `\`) as the
/// starting position, and its remaining components as names. `None` for
/// a relative path: the kernel has no working directory.
///
/// `.` among the names: `Path::components` normalizes `.` away in every
/// non-verbatim path (Unix, wasi, a plain Windows spelling), so the
/// walks below see `.` only from a Windows VERBATIM spelling
/// (`\\?\C:\ws\.\x` — nothing in it is normalized) or a drive-relative
/// one (`C:.\x` — a prefix with no root, split here as if absolute with
/// its head `.` kept). Their `.` clauses are live for exactly those
/// operator-typed spellings (pinned in `tests/paths.rs`).
pub(super) fn split_absolute(path: &Path) -> Option<(PathBuf, Vec<OsString>)> {
    // Windows: `/ws/...` is the operator's POSIX spelling (tests, CLI on
    // Unix-shaped paths). `is_absolute()` is false and `RootDir` becomes
    // `\`, which would desync the mock and refuse mint as `NotRelative`.
    #[cfg(windows)]
    if !path.is_absolute() && matches!(path.components().next(), Some(Component::RootDir)) {
        let names = path
            .components()
            .skip(1)
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        return Some((PathBuf::from("/"), names));
    }
    let mut components = path.components().peekable();
    let mut cur = PathBuf::new();
    let mut prefixed = false;
    while let Some(c) = components.peek() {
        match c {
            Component::Prefix(p) => cur.push(p.as_os_str()),
            Component::RootDir => cur.push(c.as_os_str()),
            _ => break,
        }
        prefixed = true;
        components.next();
    }
    if !prefixed {
        return None;
    }
    let names = components.map(|c| c.as_os_str().to_os_string()).collect();
    Some((cur, names))
}

/// Whether `path` is folded from the filesystem root ([`fold_absolute`]),
/// including POSIX `/…` spellings on Windows (see [`split_absolute`]).
fn enters_via_fold(path: &Path) -> bool {
    path.is_absolute()
        || (cfg!(windows) && matches!(path.components().next(), Some(Component::RootDir)))
}

/// The invocation target's directory, by the fence-aware walk (E28): the
/// target itself when it is a directory (`nml fix <dir>`), else its
/// parent. Symlinks are followed only while no `.git` fence has been
/// seen — the prefix outside every repository is the operator's — and
/// each resolved endpoint must be a directory. Below the fence the walk
/// is `lstat`-only: the first symlink component ends it, the directory
/// BEFORE the link being the target's, identically whether the link
/// dangles or not (its target is never resolved; the trust-aware walk
/// judges it later). So under a `.git` fence derivation never follows a
/// link an author could have committed. With no fence at all the regime
/// is E21's: the universe is the target's own directory, so the chain
/// ABOVE it is the operator's and is followed like any prefix — a link
/// there IS resolved, and its existence is observable through the
/// result (nothing above a no-VCS universe can govern the file; CI
/// passes `--root`) — while the target's own directory being a link is
/// refused ([`RootError::UnfencedSymlink`]) however the spelling names
/// it (`link/x`, `link/sub/../x` alike — the lexical tail decides): the
/// fence itself is never derived through a link. The leaf itself is
/// never resolved here.
fn target_dir(target: &Path, fs: &dyn PathFs) -> Result<PathBuf, RootError> {
    let (mut cur, names) = split_absolute(target).ok_or(RootError::NotAbsolute)?;
    let mut fenced = false;
    let mut i = 0;
    while i < names.len() {
        let name = &names[i];
        i += 1;
        // `.` only from a Windows verbatim or drive-relative spelling
        // (see `split_absolute`); elsewhere `components` dropped it.
        if name == "." {
            continue;
        }
        if name == ".." {
            cur.pop();
            continue;
        }
        let remaining = names[i..]
            .iter()
            .filter(|n| *n != "." && *n != "..")
            .count();
        let Some(step) = fs.child(&cur, name)? else {
            // An absent leaf still has a directory; an absent ancestor has
            // nothing under it.
            return if remaining == 0 {
                Ok(cur)
            } else {
                Err(RootError::NotADirectory)
            };
        };
        match step.kind {
            EntryKind::Dir => cur.push(step.spelling),
            EntryKind::File | EntryKind::Other => {
                return if remaining == 0 {
                    Ok(cur)
                } else {
                    Err(RootError::NotADirectory)
                };
            }
            EntryKind::Symlink => {
                if remaining == 0 {
                    // The leaf: the target itself, judged by trust later.
                    return Ok(cur);
                }
                if !fenced {
                    fenced = vcs_fenced(&cur, fs)?;
                }
                if fenced {
                    // Author territory: the directory before the link.
                    return Ok(cur);
                }
                if remaining == 1 || lexical_tail_depth(&names[i..]) == Some(1) {
                    return Err(RootError::UnfencedSymlink {
                        component: name.to_string_lossy().into_owned(),
                    });
                }
                // The operator's prefix: followed, endpoint kind-checked.
                match fs.resolve_symlink(&cur, name) {
                    Ok(Some(resolved)) => {
                        if kind_at(fs, &resolved)? != Some(EntryKind::Dir) {
                            return Err(RootError::NotADirectory);
                        }
                        cur = resolved;
                    }
                    Ok(None) => return Err(RootError::NotADirectory),
                    // No realpath on this backend: keep the spelling.
                    Err(FsError::NoRealpath) => cur.push(name),
                    Err(e) => return Err(RootError::Fs(e)),
                }
            }
        }
    }
    Ok(cur)
}

/// The depth the components after a link spell LEXICALLY — `.` dropped,
/// `..` popping — so `sub/../x.nml` is 1: the link is then the target's
/// own directory however the tail is spelled, and the unfenced walk
/// refuses it exactly as it refuses `link/x.nml` (it used to follow the
/// link and derive the universe at its target). `None` when a `..`
/// climbs above the link.
fn lexical_tail_depth(tail: &[OsString]) -> Option<usize> {
    let mut depth = 0usize;
    for name in tail {
        if name == "." {
            continue;
        }
        if name == ".." {
            depth = depth.checked_sub(1)?;
            continue;
        }
        depth += 1;
    }
    Some(depth)
}

/// Canonicalize an OPERATOR-given absolute directory by folding `child`
/// from the filesystem root, following symlinks (they are the operator's,
/// not an author's). On a backend without realpath the spelling stays as
/// given. A path ending at anything but a directory — a link to a file
/// included — is not a root.
fn canonical_dir(dir: &Path, fs: &dyn PathFs) -> Result<PathBuf, RootError> {
    let (cur, rest) = fold_absolute(dir, fs, None).map_err(|e| match e {
        Fold::Absent | Fold::NotADir => RootError::NotADirectory,
        Fold::NotAbsolute => RootError::NotAbsolute,
        Fold::Fs(e) => RootError::Fs(e),
    })?;
    debug_assert!(rest.is_empty(), "no stop path: the whole path folds");
    Ok(cur)
}

/// Why an absolute-path fold stopped short.
enum Fold {
    NotAbsolute,
    Absent,
    NotADir,
    Fs(FsError),
}

/// Fold `child` over an absolute path's components from the filesystem
/// root, following symlinks, until the path is exhausted or the position
/// ENTERS `stop` — at it or strictly inside it, an operator link into the
/// root's interior included (E28): the components under the root, those
/// the link skipped over and the authored remainder alike, are returned
/// for the policy walk, which never follows a link the trust forbids.
/// AT the root exactly, a `.`/`..` still belongs to the operator's
/// prefix — `/ws/../ws/x.nml`, or `nml check ../ws/x.nml` from the
/// root's own directory, spell a path INSIDE the root through its
/// canonical parent — so the fold consumes it (`cur` is canonical there,
/// the pop is physical) and stops at the first plain component under the
/// root; strictly inside, every component is the policy walk's, so
/// nothing is resolved once inside. Used to canonicalize operator paths
/// and to enter the root from an absolute target: the prefix OUTSIDE the
/// root is operator territory. A symlink at the END of the path is
/// kind-checked: a link to a file is not a directory.
fn fold_absolute(
    path: &Path,
    fs: &dyn PathFs,
    stop: Option<&Path>,
) -> Result<(PathBuf, Vec<OsString>), Fold> {
    let (mut cur, names) = split_absolute(path).ok_or(Fold::NotAbsolute)?;
    let mut i = 0;
    while i < names.len() {
        let name = &names[i];
        if let Some(stop) = stop {
            if cur.starts_with(stop) {
                // `.` here only from a Windows verbatim spelling (see
                // `split_absolute`); `..` from any platform.
                let dot = name == "." || name == "..";
                if !(dot && cur.as_path() == stop) {
                    break;
                }
            }
        }
        i += 1;
        if name == "." {
            continue;
        }
        if name == ".." {
            cur.pop();
            continue;
        }
        let last = i == names.len();
        match fs.child(&cur, name).map_err(Fold::Fs)? {
            None => return Err(Fold::Absent),
            Some(step) => match step.kind {
                EntryKind::Dir => cur.push(step.spelling),
                EntryKind::Symlink => match fs.resolve_symlink(&cur, name) {
                    Ok(Some(resolved)) => {
                        if last && kind_at(fs, &resolved).map_err(Fold::Fs)? != Some(EntryKind::Dir)
                        {
                            return Err(Fold::NotADir);
                        }
                        cur = resolved;
                    }
                    Ok(None) => return Err(Fold::Absent),
                    // No realpath on this backend: keep the spelling.
                    Err(FsError::NoRealpath) => cur.push(name),
                    Err(e) => return Err(Fold::Fs(e)),
                },
                EntryKind::File | EntryKind::Other => {
                    return Err(if last { Fold::NotADir } else { Fold::Absent });
                }
            },
        }
    }
    let mut rest: Vec<OsString> = names[i..].to_vec();
    let Some(stop) = stop else {
        return Ok((cur, rest));
    };
    // Entered the root — possibly strictly inside it: the components the
    // fold landed under the root are the policy walk's, not the fold's.
    let inside = cur.strip_prefix(stop).map_err(|_| Fold::Absent)?;
    let mut under: Vec<OsString> = inside
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    under.append(&mut rest);
    Ok((stop.to_path_buf(), under))
}

// ──────────────────────────────────────────────────────────────── keys ──

/// Canonical workspace-relative, `/`-only, byte-exact, no `.`/`..`/empty
/// components, never absolute. The ONE spelling `InstanceId.source_path`,
/// `Diagnostic.source`, `Related.source` and grant matching all use. The
/// empty key names the root directory itself (anchors).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceKey(String);

impl std::fmt::Display for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How much the walk trusts the tree under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Author-writable content under a binding: a symlink component halts
    /// the walk (NML2083 form 1) and an unverifiable spelling fails closed
    /// (form 2). [`PathFs::resolve_symlink`] is never called.
    Closed,
    /// A developer's own repo: symlinks are followed (their targets must
    /// still lie under the root), unverifiable spellings are tolerated
    /// and reported.
    Open,
}

/// What minting learned about symlinks on the way to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkVerdict {
    None,
    /// The first symlinked component, by index into the authored path.
    Through(usize),
    /// A spelling could not be verified on this backend (open trust only;
    /// closed trust turns this into [`PathError::Unverifiable`]).
    Unverifiable,
}

/// Typed key failures — the A7 table maps each to a code or a CLI error
/// (`workspace::diag`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// P1: not a quoted relative path (a scheme, a leading separator, a
    /// drive prefix, or no file name at all).
    NotRelative,
    /// P2: the path resolves outside the workspace root. Carries the
    /// AUTHORED path only — never a resolved target (E26).
    Escapes {
        authored: String,
    },
    /// More than [`MAX_COMPONENTS`] components — authored or minted.
    Depth,
    /// A path component is not UTF-8: it cannot be a key.
    NotUtf8,
    /// A path component bears a separator (`ev\il` on unix — a legal
    /// name git tracks): no key carries one, so the path names nothing
    /// the kernel can read. Refused where the key is MINTED — the read
    /// used to refuse it one step later (`is not a plain path
    /// component`), after a key with a `\\` in it had been judged.
    NotPlain {
        component: String,
    },
    /// P4 (closed trust): the component named `component` is a symlink.
    /// `key` is the lexical spelling for the message — the target was
    /// never resolved, so the message is identical whether or not it
    /// exists.
    SymlinkComponent {
        component: String,
        key: SourceKey,
    },
    /// Closed trust on a backend that cannot verify on-disk spelling
    /// (NML2083 form 2).
    Unverifiable {
        key: SourceKey,
    },
    Fs(FsError),
}

impl From<FsError> for PathError {
    fn from(e: FsError) -> Self {
        Self::Fs(e)
    }
}

/// A minted key: P1+P2 done, the leaf never probed. `verify` is the only
/// way to learn whether the file exists — type-state: you cannot verify a
/// key you did not mint, and ancestor verdicts are never recomputed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyed {
    pub key: SourceKey,
    pub via_symlink: SymlinkVerdict,
    /// The canonical parent directory when the whole parent chain exists
    /// (the only case in which the leaf can exist).
    parent: Option<PathBuf>,
    leaf: String,
    trust: Trust,
}

/// A verified key (post-allow, A11): the FINAL key is the one that names
/// what will be read. `respelled` ⇒ the caller re-judges it deny-first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub key: SourceKey,
    pub kind: EntryKind,
    pub via_symlink: SymlinkVerdict,
    pub respelled: bool,
}

/// The ONE way a name becomes a key component: UTF-8
/// ([`PathError::NotUtf8`]) and plain ([`PathError::NotPlain`]) — every
/// minted, halted, respelled or lexical key is assembled from components
/// this fn admitted, so "a key never carries a separator" is a property
/// of the type's construction, not of each caller's care. (`.` and `..`
/// are handled by the walks before they reach here; a name that IS one
/// is a caller bug, refused all the same.)
fn component(name: &OsStr) -> Result<&str, PathError> {
    let s = name.to_str().ok_or(PathError::NotUtf8)?;
    if crate::glob::is_plain_name(s) {
        Ok(s)
    } else {
        Err(PathError::NotPlain {
            component: s.to_string(),
        })
    }
}

impl SourceKey {
    /// The root directory itself.
    pub fn root() -> Self {
        Self(String::new())
    }

    /// A key from its own spelling — the way a key that LEFT the kernel
    /// (a diagnostic's `source`) comes back: every component plain, at
    /// most [`MAX_COMPONENTS`] of them, never absolute. `None` for
    /// anything else; minting, not parsing, is how a path becomes a key.
    pub fn checked(s: &str) -> Option<Self> {
        if s.is_empty() {
            return Some(Self::root());
        }
        let mut count = 0usize;
        for component in s.split('/') {
            if !crate::glob::is_plain_name(component) {
                return None;
            }
            count += 1;
        }
        (count <= MAX_COMPONENTS).then(|| Self(s.to_string()))
    }

    /// The key `path` spells under `root` LEXICALLY — the ONE lexical
    /// keying, for both front ends: the operator-side prefix above the
    /// root folded through `fs` (a `/tmp` spelled for `/private/tmp`),
    /// the relative components with `.` dropped and `..` popped (the
    /// spelling [`Self::mint`] would settle on, E28), nothing probed
    /// below the root. For the question that must be answered BEFORE any
    /// probe — does the path sit under a budget unit the walk refused
    /// (nothing under such a unit is ever `lstat`ed) — and for the
    /// editor's config and coverage lookups (which probe nothing). `None`
    /// where the spelling escapes the root, is non-UTF-8, bears a
    /// separator inside a component, or is too deep: the probing `mint`
    /// then says which. (The editor kept a second lexical keying that
    /// refused `..` and turned a unix `ev\il` into two components.)
    pub fn under(root: &WorkspaceRoot, path: &Path, fs: &dyn PathFs) -> Option<Self> {
        let authored = || path.to_string_lossy().replace('\\', "/");
        let rel = relative_components(root, path, fs, &authored).ok()?;
        let mut comps: Vec<String> = Vec::new();
        for name in &rel {
            if name == "." {
                continue;
            }
            if name == ".." {
                comps.pop()?;
                continue;
            }
            comps.push(component(name).ok()?.to_string());
        }
        (comps.len() <= MAX_COMPONENTS).then(|| Self(comps.join("/")))
    }

    /// P2 + P4: mint the key for `path` under `root`. A relative `path` is
    /// root-relative (an authored ref); an absolute one enters the root
    /// through its operator-side prefix (symlinks followed there — the
    /// prefix outside the root is not author-writable) and is then walked
    /// by `trust` under it. Parent components are folded through
    /// [`LstatFs::child`] from the canonical root; `..` is the lexical
    /// parent of the canonical prefix (above the root ⇒ `Escapes`); the
    /// leaf is never probed.
    pub fn mint(
        root: &WorkspaceRoot,
        path: &Path,
        fs: &dyn PathFs,
        trust: Trust,
    ) -> Result<Keyed, PathError> {
        let authored = || path.to_string_lossy().replace('\\', "/");
        let rel = relative_components(root, path, fs, &authored)?;
        let leaf = match rel.last() {
            Some(name) if name != "." && name != ".." => name.clone(),
            _ => return Err(PathError::NotRelative),
        };
        let leaf_index = rel.len() - 1;
        let leaf_str = component(&leaf)?.to_string();
        let walked = walk_components(root, &rel, leaf_index, Oracle::new(fs, trust), &authored)?;
        if walked.comps.len() >= MAX_COMPONENTS {
            return Err(PathError::Depth);
        }
        let Walked {
            cur,
            mut comps,
            resolved,
            verdict,
        } = walked;
        let parent = (resolved == comps.len()).then_some(cur);
        comps.push(leaf_str.clone());
        Ok(Keyed {
            key: SourceKey(comps.join("/")),
            via_symlink: verdict,
            parent,
            leaf: leaf_str,
            trust,
        })
    }

    /// What an OPERATOR-given path names under `root` (E33's DRY
    /// end-state, built in E35): the one classification a verb that
    /// expands directory arguments runs (`nml fix <dir>`). The same entry
    /// through the operator's prefix, the same trust-aware walk and the
    /// same halts as [`SourceKey::mint`] — so an argument is a directory
    /// exactly when the kernel can reach a directory there — but EVERY
    /// component is walked, the last included, and a `.`/`..` last
    /// component is applied like any other: an argv path is the
    /// operator's own, not an authored reference a grant has yet to
    /// allow. A halt (closed trust meeting a link, an unverifiable
    /// spelling), an escape, a denied or looping prefix is the `Err` the
    /// caller reports through `mint` + `verify` — never a directory.
    /// Under [`Trust::Open`] a link is followed and its endpoint
    /// kind-checked (E31 option (b) — the caller chooses the trust).
    pub fn classify(
        root: &WorkspaceRoot,
        path: &Path,
        fs: &dyn PathFs,
        trust: Trust,
    ) -> Result<Endpoint, PathError> {
        let authored = || path.to_string_lossy().replace('\\', "/");
        let rel = relative_components(root, path, fs, &authored)?;
        let walked = walk_components(root, &rel, rel.len(), Oracle::new(fs, trust), &authored)?;
        if walked.comps.is_empty() {
            return Ok(Endpoint::Root);
        }
        if walked.comps.len() > MAX_COMPONENTS {
            return Err(PathError::Depth);
        }
        // Every component existed as a directory (or, under open trust,
        // resolved to one): the endpoint is the resolved position, whose
        // kind is confirmed by one probe — a link to a FILE under open
        // trust resolves the prefix without being a directory.
        if walked.resolved == walked.comps.len()
            && kind_at(fs, &walked.cur)? == Some(EntryKind::Dir)
        {
            return Ok(Endpoint::Dir(SourceKey(walked.comps.join("/"))));
        }
        Ok(Endpoint::Other)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key as a DIRECTORY reads to a human: the root key is empty and
    /// spells `.` — the one spelling the `nml binding` anchor row and the
    /// NML2089 sentence share.
    pub fn dir_label(&self) -> &str {
        if self.0.is_empty() { "." } else { &self.0 }
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The directory holding this key (the root key for a top-level name).
    pub fn dir(&self) -> SourceKey {
        match self.0.rfind('/') {
            Some(i) => SourceKey(self.0[..i].to_string()),
            None => SourceKey::root(),
        }
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    /// `self` (a directory key) is a strict ancestor of `other`.
    pub(crate) fn is_strict_ancestor_of(&self, other: &SourceKey) -> bool {
        if self.0.is_empty() {
            return !other.0.is_empty();
        }
        other.0.len() > self.0.len()
            && other.0.starts_with(&self.0)
            && other.0.as_bytes()[self.0.len()] == b'/'
    }

    /// `self` (a directory key) is an ancestor of `other` or `other` itself.
    pub fn contains(&self, other: &SourceKey) -> bool {
        self == other || self.is_strict_ancestor_of(other)
    }

    /// The directory holding this key, as a borrowed prefix (the empty
    /// prefix for a top-level key) — [`SourceKey::dir`] without the
    /// allocation, for the settlement's hot filters.
    fn dir_str(&self) -> &str {
        match self.0.rfind('/') {
            Some(i) => &self.0[..i],
            None => "",
        }
    }

    /// `dir(self)` is a strict ancestor of `other` — `self.dir()
    /// .is_strict_ancestor_of(other)` without minting the directory key.
    pub(crate) fn dir_is_strict_ancestor_of(&self, other: &SourceKey) -> bool {
        let dir = self.dir_str();
        if dir.is_empty() {
            return !other.0.is_empty();
        }
        other.0.len() > dir.len()
            && other.0.starts_with(dir)
            && other.0.as_bytes()[dir.len()] == b'/'
    }

    /// `dir(self)` is an ancestor of `other` or `other` itself —
    /// `self.dir().contains(other)` without minting the directory key.
    pub(crate) fn dir_contains(&self, other: &SourceKey) -> bool {
        self.dir_str() == other.0 || self.dir_is_strict_ancestor_of(other)
    }

    /// `other` relative to `self` (a directory key), when contained.
    pub fn relative_to(&self, dir: &SourceKey) -> Option<&str> {
        if dir.0.is_empty() {
            return Some(&self.0);
        }
        if dir.is_strict_ancestor_of(self) {
            return Some(&self.0[dir.0.len() + 1..]);
        }
        None
    }

    /// `self/name` for a PLAIN entry name — a listing's, a verified
    /// spelling's. Not a parser: a name with a separator or a dot
    /// component is a caller bug ([`SourceKey::checked`] parses).
    pub fn join(&self, name: &str) -> SourceKey {
        debug_assert!(
            crate::glob::is_plain_name(name),
            "join takes a plain name, got {name:?}"
        );
        if self.0.is_empty() {
            SourceKey(name.to_string())
        } else {
            SourceKey(format!("{}/{name}", self.0))
        }
    }

    /// The key of the subdirectory `name` of this directory key — `None`
    /// at the component bound: a directory at depth [`MAX_COMPONENTS`]
    /// holds nothing keyable (a key has at most that many components),
    /// and no manifest below it governs a keyable key (R5′: its own
    /// subtree only) — an exact skip, never a truncation. The one
    /// descent rule the discovery walk and the hidden audit share.
    pub(crate) fn child_dir(&self, name: &str) -> Option<SourceKey> {
        (self.depth() + 1 < MAX_COMPONENTS).then(|| self.join(name))
    }
}

/// What [`SourceKey::classify`] found at an operator-given path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// The root directory itself (`nml fix .`, `vendor/..`).
    Root,
    /// An existing directory under the root, at its verified key.
    Dir(SourceKey),
    /// Not a directory the walk could reach — a file, a device, an absent
    /// path, a dangling or file-targeted link under open trust. The
    /// caller treats it as a file candidate and lets `mint` + `verify`
    /// judge it.
    Other,
}

/// The components of `path` under `root`, before any walk: a relative
/// path is root-relative; an absolute one enters the root through its
/// operator-side prefix ([`fold_absolute`]). The authored bound (E28):
/// refused before the first probe.
fn relative_components(
    root: &WorkspaceRoot,
    path: &Path,
    fs: &dyn PathFs,
    authored: &dyn Fn() -> String,
) -> Result<Vec<OsString>, PathError> {
    let rel: Vec<OsString> = if enters_via_fold(path) {
        fold_absolute(path, fs, Some(root.path()))
            .map(|(_, rest)| rest)
            .map_err(|e| match e {
                Fold::Absent | Fold::NotADir | Fold::NotAbsolute => PathError::Escapes {
                    authored: authored(),
                },
                Fold::Fs(e) => PathError::Fs(e),
            })?
    } else {
        path.components()
            .map(|c| match c {
                Component::Normal(n) => Ok(n.to_os_string()),
                Component::CurDir => Ok(OsString::from(".")),
                Component::ParentDir => Ok(OsString::from("..")),
                Component::RootDir | Component::Prefix(_) => Err(PathError::NotRelative),
            })
            .collect::<Result<_, _>>()?
    };
    if rel.len() > MAX_COMPONENTS {
        return Err(PathError::Depth);
    }
    Ok(rel)
}

/// The oracle the policy walk holds, BY TRUST (E35): a closed walk holds
/// the non-resolving half only, so it has no way to resolve a link —
/// "closed trust never resolves a symlink" is a property of this type,
/// not of a probe log. The upcast `&dyn PathFs → &dyn LstatFs` is stable
/// since 1.86, the MSRV.
#[derive(Clone, Copy)]
enum Oracle<'a> {
    Closed(&'a dyn LstatFs),
    Open(&'a dyn PathFs),
}

impl<'a> Oracle<'a> {
    fn new(fs: &'a dyn PathFs, trust: Trust) -> Self {
        match trust {
            Trust::Closed => Self::Closed(fs),
            Trust::Open => Self::Open(fs),
        }
    }

    fn lstat(self) -> &'a dyn LstatFs {
        match self {
            Self::Closed(fs) => fs,
            Self::Open(fs) => fs,
        }
    }
}

/// The state of the trust-aware walk after `count` components.
struct Walked {
    /// `root/comps[..resolved]`: the probed, existing directory prefix.
    cur: PathBuf,
    comps: Vec<String>,
    resolved: usize,
    verdict: SymlinkVerdict,
}

/// The trust-aware walk (E26, E28) over `all[..count]` from the canonical
/// root: `child` per component, `lstat` before any resolution. `cur` is
/// `root/comps[..resolved]`, the probed, existing directory prefix of the
/// key. Components past it are lexical — absent, or under a file — until
/// a `..` pops back to the resolved prefix, where probing RESUMES (E28):
/// `tenants/nobody/../cu/lib` meets the `lib` link exactly as
/// `tenants/cu/lib` does. A closed-trust halt spells its key from the
/// unwalked tail `all[i + 1..]` (the leaf included, for `mint`). Shared
/// by [`SourceKey::mint`] (parents only) and [`SourceKey::classify`]
/// (every component) — one walk, one set of halts (E35).
fn walk_components(
    root: &WorkspaceRoot,
    all: &[OsString],
    count: usize,
    oracle: Oracle<'_>,
    authored: &dyn Fn() -> String,
) -> Result<Walked, PathError> {
    let fs = oracle.lstat();
    let mut cur = root.path().to_path_buf();
    let mut comps: Vec<String> = Vec::new();
    let mut resolved = 0usize;
    let mut verdict = SymlinkVerdict::None;
    for (i, name) in all[..count].iter().enumerate() {
        if name == "." {
            continue;
        }
        if name == ".." {
            if comps.pop().is_none() {
                return Err(PathError::Escapes {
                    authored: authored(),
                });
            }
            if resolved > comps.len() {
                cur.pop();
                resolved = comps.len();
            }
            continue;
        }
        if comps.len() >= MAX_COMPONENTS {
            return Err(PathError::Depth);
        }
        let name_str = component(name)?;
        if resolved < comps.len() {
            comps.push(name_str.to_string());
            continue;
        }
        match fs.child(&cur, name)? {
            None => comps.push(name_str.to_string()),
            Some(step) => match step.kind {
                EntryKind::Symlink => match oracle {
                    // P4 before P3: halt HERE. The lexical remainder is
                    // message material only; the target is never
                    // resolved — this arm holds no resolver.
                    Oracle::Closed(_) => {
                        return Err(PathError::SymlinkComponent {
                            component: name_str.to_string(),
                            key: halt_key(comps, name_str, &all[i + 1..])?,
                        });
                    }
                    Oracle::Open(open) => {
                        first(&mut verdict, SymlinkVerdict::Through(i));
                        match open.resolve_symlink(&cur, name) {
                            Ok(Some(target)) => {
                                let rel = target.strip_prefix(root.path()).map_err(|_| {
                                    PathError::Escapes {
                                        authored: authored(),
                                    }
                                })?;
                                comps = rel
                                    .components()
                                    .map(|c| component(c.as_os_str()).map(str::to_string))
                                    .collect::<Result<_, _>>()?;
                                if comps.len() > MAX_COMPONENTS {
                                    return Err(PathError::Depth);
                                }
                                cur = target;
                                resolved = comps.len();
                            }
                            // Dangling: nothing exists under it.
                            Ok(None) => comps.push(name_str.to_string()),
                            Err(FsError::NoRealpath) => {
                                first(&mut verdict, SymlinkVerdict::Unverifiable);
                                comps.push(name_str.to_string());
                                cur.push(name);
                                resolved = comps.len();
                            }
                            Err(e) => return Err(PathError::Fs(e)),
                        }
                    }
                },
                EntryKind::Dir | EntryKind::File | EntryKind::Other => {
                    if !step.spelling_verified {
                        match oracle {
                            Oracle::Closed(_) => {
                                return Err(PathError::Unverifiable {
                                    key: halt_key(comps, name_str, &all[i + 1..])?,
                                });
                            }
                            Oracle::Open(_) => first(&mut verdict, SymlinkVerdict::Unverifiable),
                        }
                    }
                    let spelled = component(&step.spelling)?;
                    comps.push(spelled.to_string());
                    // Nothing exists under a file: only a directory
                    // extends the resolved prefix.
                    if step.kind == EntryKind::Dir {
                        cur.push(&step.spelling);
                        resolved = comps.len();
                    }
                }
            },
        }
    }
    Ok(Walked {
        cur,
        comps,
        resolved,
        verdict,
    })
}

/// The first verdict wins: the index of the FIRST symlinked component is
/// what the verdict reports.
fn first(verdict: &mut SymlinkVerdict, v: SymlinkVerdict) {
    if *verdict == SymlinkVerdict::None {
        *verdict = v;
    }
}

/// The key a closed-trust HALT reports (message material only): the
/// resolved prefix, the halting component, and the unwalked tail spelled
/// lexically — `.` dropped, `..` popping whatever precedes it (the
/// halting component included: `lib/../lib/x` spells `lib/x`), never
/// resolved. Still a key: the component bound holds (the fuzz target
/// found a 68-component halt key before this check existed; since the
/// authored bound in [`relative_components`] the check is defence in
/// depth — unreachable through `mint`/`classify`, pinned directly).
pub(super) fn halt_key(
    mut comps: Vec<String>,
    at: &str,
    tail: &[OsString],
) -> Result<SourceKey, PathError> {
    comps.push(at.to_string());
    for name in tail {
        if name == "." {
            continue;
        }
        if name == ".." {
            comps.pop();
            continue;
        }
        comps.push(component(name)?.to_string());
    }
    if comps.len() > MAX_COMPONENTS {
        return Err(PathError::Depth);
    }
    Ok(SourceKey(comps.join("/")))
}

impl Keyed {
    /// Post-allow verification (A11): probe the leaf under the verified
    /// parent. `Ok(None)` = no such file. Under closed trust a symlink
    /// leaf is [`PathError::SymlinkComponent`] (never resolved) and an
    /// unverifiable spelling is [`PathError::Unverifiable`]; a leaf the
    /// filesystem spells differently is reported `respelled` — the caller
    /// re-judges the FINAL key deny-first. Typed over the non-resolving
    /// oracle (E35): verification never resolves a link under either
    /// trust — an open-trust symlink leaf is REPORTED, not followed.
    pub fn verify(&self, fs: &dyn LstatFs) -> Result<Option<Verified>, PathError> {
        let Some(parent) = &self.parent else {
            return Ok(None);
        };
        let Some(step) = fs.child(parent, OsStr::new(&self.leaf))? else {
            return Ok(None);
        };
        let leaf_index = self.key.depth().saturating_sub(1);
        if step.kind == EntryKind::Symlink {
            return match self.trust {
                Trust::Closed => Err(PathError::SymlinkComponent {
                    component: self.leaf.clone(),
                    key: self.key.clone(),
                }),
                Trust::Open => Ok(Some(Verified {
                    key: self.key.clone(),
                    kind: EntryKind::Symlink,
                    via_symlink: match self.via_symlink {
                        SymlinkVerdict::None => SymlinkVerdict::Through(leaf_index),
                        earlier => earlier,
                    },
                    respelled: false,
                })),
            };
        }
        let mut via_symlink = self.via_symlink;
        if !step.spelling_verified {
            match self.trust {
                Trust::Closed => {
                    return Err(PathError::Unverifiable {
                        key: self.key.clone(),
                    });
                }
                Trust::Open => {
                    if via_symlink == SymlinkVerdict::None {
                        via_symlink = SymlinkVerdict::Unverifiable;
                    }
                }
            }
        }
        let spelled = component(&step.spelling)?;
        let respelled = spelled != self.leaf;
        let key = if respelled {
            self.key.dir().join(spelled)
        } else {
            self.key.clone()
        };
        Ok(Some(Verified {
            key,
            kind: step.kind,
            via_symlink,
            respelled,
        }))
    }
}
