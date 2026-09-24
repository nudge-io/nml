//! Pins on the scripted [`MockFs`] that need ambient `std::os` (kept out of
//! `mock.rs` so the workspace ambient-authority ratchet stays clean).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::fs::{EntryKind, LstatFs};
use crate::workspace::mock::MockFs;

/// A scripted name that is not UTF-8 must survive `script_path` and
/// `list_dir` byte-for-byte — otherwise discover sees a plain name.
#[cfg(unix)]
#[test]
fn a_non_utf8_scripted_name_lists_without_lossy_folding() {
    let odd = OsStr::from_bytes(b"n\xff.nml");
    let path = Path::new("/ws").join(odd);
    let fs = MockFs::new().file(&path);
    let listed = fs.list_dir(Path::new("/ws")).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0.as_os_str(), odd);
    assert_eq!(listed[0].1, EntryKind::File);
    assert!(odd.to_str().is_none());
}
