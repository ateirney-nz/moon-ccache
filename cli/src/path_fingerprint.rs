//! Path fingerprinting for cache input tracking.
//!
//! Provides types and functions for creating fingerprints of file system state
//! to validate whether cached outputs are still valid.
//!
//! Modelled after <https://github.com/voidzero-dev/vite-task/blob/main/crates/vite_task/src/session/execute/fingerprint.rs>.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead},
    path::Path,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::Xxh3;

/// Fingerprint for a single path (file or directory).
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub(crate) enum PathFingerprint {
    /// Path was not found when fingerprinting.
    NotFound,
    /// File content hash using XXH3-64 plus byte length.
    ///
    /// The byte length is stored alongside the hash so that execution validation
    /// can run a fast stat-only pass (size check) before the more expensive
    /// content-hash pass.
    File {
        /// XXH3-64 digest of the file's full contents.
        hash: u64,
        /// File size in bytes.
        size: u64,
    },
    /// Directory fingerprint.
    ///
    /// - `None`: the directory was opened (e.g. for `openat`) but its entries were not
    ///   enumerated — only presence is tracked. Produced when fspy reports `READ` but
    ///   not `READ_DIR` on a directory path.
    /// - `Some(_)`: `readdir`/`getdents` was called; the entry listing is captured so
    ///   additions or removals are detected on the next cache lookup. Produced when fspy
    ///   reports `READ_DIR` on the path.
    Directory(Option<BTreeMap<String, DirEntryKind>>),
}

/// Kind of directory entry.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub(crate) enum DirEntryKind {
    File,
    Dir,
    Symlink,
}

/// Fingerprint a single path (file or directory).
///
/// - Files: streams content through XXH3-64 and returns [`PathFingerprint::File`].
/// - Directories: returns [`PathFingerprint::Directory`]. When `read_dir` is `true`
///   (fspy reported `READ_DIR` on this path), the entry listing is captured. When
///   `false`, only presence is recorded and the inner map is `None`.
/// - Missing paths: returns [`PathFingerprint::NotFound`].
///
/// Symlinks are followed so the fingerprint reflects the resolved target.
pub(crate) fn fingerprint_path(path: &Path, read_dir: bool) -> Result<PathFingerprint> {
    // Stat first to distinguish files from directories without relying on
    // platform-specific open-directory error codes.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound && e.kind() != io::ErrorKind::NotADirectory {
                eprintln!(
                    "ccache: unexpected error stat-ing {}: {}",
                    path.display(),
                    e
                );
            }
            return Ok(PathFingerprint::NotFound);
        }
    };

    if meta.is_dir() {
        return fingerprint_directory(path, read_dir);
    }

    // Regular file (or symlink resolved to a file).
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                eprintln!("ccache: unexpected error opening {}: {}", path.display(), e);
            }
            return Ok(PathFingerprint::NotFound);
        }
    };

    fingerprint_file(io::BufReader::new(file))
}

/// Stream a file through XXH3-64 and return its fingerprint (hash + byte length).
fn fingerprint_file(mut reader: impl BufRead) -> Result<PathFingerprint> {
    let mut hasher = Xxh3::new();
    let mut size = 0u64;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        hasher.update(buf);
        size += n as u64;
        reader.consume(n);
    }
    Ok(PathFingerprint::File {
        hash: hasher.digest(),
        size,
    })
}

/// Fingerprint a directory path.
///
/// When `read_dir_entries` is `false`, returns `Directory(None)` — only presence is
/// tracked. When `true`, lists entries into a sorted [`BTreeMap`] and returns
/// `Directory(Some(_))`. `.DS_Store` entries are skipped. Note: `read_dir` never yields
/// `.` or `..` on any supported platform, so those do not need to be filtered.
fn fingerprint_directory(path: &Path, read_dir_entries: bool) -> Result<PathFingerprint> {
    if !read_dir_entries {
        return Ok(PathFingerprint::Directory(None));
    }

    let mut entries = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == ".DS_Store" {
            continue;
        }

        let file_type = entry.file_type()?;
        let kind = if file_type.is_file() {
            DirEntryKind::File
        } else if file_type.is_dir() {
            DirEntryKind::Dir
        } else {
            DirEntryKind::Symlink
        };

        entries.insert(name_str.into_owned(), kind);
    }
    Ok(PathFingerprint::Directory(Some(entries)))
}
