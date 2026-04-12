//! Trace manifest type and construction from fspy path accesses.

use anyhow::Result;
use fspy::AccessMode;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::fingerprint::{PathFingerprint, PathRead, fingerprint_path};
use crate::path_filter::PathFilter;
use crate::paths::to_relative_path;

/// Workspace-relative paths → path fingerprints for all tracked inputs (files and
/// directories) that were read but not written during the traced run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TraceManifest {
    pub(crate) inputs: HashMap<String, PathFingerprint>,
}

/// Interpret raw fspy path accesses into a cache [`TraceManifest`].
///
/// - **Pass 1**: collect every path that was written, so reads of the same path are
///   excluded from inputs.
/// - **Pass 2**: from paths that were read (but not written) under `workspace_root`,
///   deduplicate and apply `input_filter`. Both files and directories are tracked.
///   For each deduplicated path the `READ_DIR` flag is OR-ed across all accesses so that
///   a single `readdir` call on any visit causes the directory's entry listing to be
///   fingerprinted rather than just its presence.
/// - **Pass 3**: fingerprint the surviving input paths in parallel via rayon, passing
///   the per-path `read_dir_entries` flag to [`fingerprint_path`].
pub(crate) fn build_trace_manifest(
    path_accesses: &[(AccessMode, PathBuf)],
    workspace_root: &Path,
    input_filter: Option<&PathFilter>,
) -> Result<TraceManifest> {
    // Pass 1: record written paths so reads of the same path are excluded from inputs.
    let written_paths: std::collections::HashSet<&PathBuf> = path_accesses
        .iter()
        .filter(|(mode, _)| mode.contains(AccessMode::WRITE))
        .map(|(_, path)| path)
        .collect();

    // Pass 2: collect unique read-only paths under the workspace root.
    // Value: (PathRead, canonical path ref).
    // `read_dir_entries` is OR-ed across all accesses so a single READ_DIR on any
    // visit upgrades the fingerprint from presence-only to full entry listing.
    let mut seen: HashMap<String, (PathRead, &PathBuf)> = HashMap::new();
    for (mode, path) in path_accesses {
        let is_read = mode.intersects(AccessMode::READ | AccessMode::READ_DIR);
        if is_read && !written_paths.contains(path) && path.starts_with(workspace_root) {
            let rel = to_relative_path(path, workspace_root);
            if let Some(filter) = input_filter
                && !filter.allows(&rel)
            {
                continue;
            }
            let read_dir_entries = mode.contains(AccessMode::READ_DIR);
            seen.entry(rel)
                // If a later access on the same path has READ_DIR, upgrade to listing.
                .and_modify(|(pr, _)| pr.read_dir_entries |= read_dir_entries)
                .or_insert((PathRead { read_dir_entries }, path));
        }
    }

    // Pass 3: fingerprint in parallel.
    let inputs: HashMap<String, PathFingerprint> = seen
        .into_par_iter()
        .map(|(rel, (path_read, path))| {
            let fp = fingerprint_path(path, path_read)?;
            Ok((rel, fp))
        })
        .collect::<Result<_>>()?;

    Ok(TraceManifest { inputs })
}
