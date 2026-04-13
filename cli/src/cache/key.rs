//! Coarse cache key computation.

use anyhow::Result;
use rayon::prelude::*;
use std::io::Read;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::Xxh3;

use crate::path_utils::to_relative_path;
use crate::task_info::TaskInfo;

/// Compute the execution key — the manifest directory name under which executions
/// are stored.
///
/// Built from static task metadata and environment; no runtime file tracing. All
/// components are fed into an **XXH3** hasher and the result is formatted as a
/// 16-character lowercase hex string. The key covers:
/// - The Moon target name
/// - The exact command line
/// - The XXH3 hash of declared input file contents (from the Moon snapshot)
/// - Task-declared `env` values (sorted by key for stability)
/// - Runtime values of `input_env` variables (resolved from the process environment)
/// - Any `--exclude` patterns
pub(crate) fn execution_key(
    target: &str,
    command: &[String],
    task_info: &TaskInfo,
    workspace_root: &Path,
    excludes: &[String],
) -> Result<String> {
    let mut hasher = Xxh3::new();
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    for arg in command {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\x01");

    let input_key = paths_content_hash(&task_info.input_files, workspace_root)?;
    hasher.update(input_key.as_bytes());
    hasher.update(b"\x01");

    // Task-declared env values (BTreeMap is already sorted).
    for (key, value) in &task_info.env {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\x01");

    // input_env: runtime values resolved from the process environment (pre-sorted in TaskInfo).
    for key in &task_info.input_env {
        let value = std::env::var(key).unwrap_or_default();
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"\x01");

    for pat in excludes {
        hasher.update(pat.as_bytes());
        hasher.update(b"\0");
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// Hash the contents of `files` (absolute paths from `Task::get_input_files`).
///
/// Uses workspace-relative paths as keys so the digest is stable regardless of where
/// the workspace is checked out. Hashing is parallelised via rayon.
fn paths_content_hash(files: &[PathBuf], workspace_root: &Path) -> Result<String> {
    let mut hashes: Vec<(String, u64)> = files
        .par_iter()
        .map(|p| {
            let rel = to_relative_path(p, workspace_root);
            let hash = hash_file(p)?;
            Ok((rel, hash))
        })
        .collect::<Result<Vec<_>>>()?;

    // par_iter does not preserve order; sort by path for a deterministic digest.
    hashes.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Xxh3::new();
    for (rel, hash) in &hashes {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        hasher.update(&hash.to_le_bytes());
        hasher.update(b"\0");
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// Stream `path` through XXH3-64 and return the raw digest.
fn hash_file(path: &Path) -> Result<u64> {
    let mut hasher = Xxh3::new();
    let mut file = fs_err::File::open(path)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.digest())
}
