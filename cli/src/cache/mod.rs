//! Execution store, content-addressed storage (CAS), and execution key computation.

mod cas;
mod execution;
mod key;

pub(crate) use execution::Execution;
pub(crate) use key::execution_key;

use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::path_fingerprint::{PathFingerprint, fingerprint_path};

pub(crate) const SCHEMA_VERSION: u32 = 1;

const MAX_EXECUTIONS: usize = 10;

/// Filesystem-backed cache: execution store for trace manifest JSON files and a
/// content-addressed store for output archives.
#[derive(Clone)]
pub(crate) struct Cache {
    manifests_dir: PathBuf,
    cas_dir: PathBuf,
    verbose: bool,
}

impl Cache {
    /// Create the cache rooted at `root`, creating subdirectories as needed.
    pub(crate) fn new(root: &Path, verbose: bool) -> Result<Self> {
        let manifests_dir = root.join("manifests");
        let cas_dir = root.join("cas");
        fs_err::create_dir_all(&manifests_dir)?;
        fs_err::create_dir_all(&cas_dir)?;
        Ok(Self {
            manifests_dir,
            cas_dir,
            verbose,
        })
    }

    fn log_verbose(&self, args: std::fmt::Arguments<'_>) {
        if self.verbose {
            eprintln!("{args}");
        }
    }

    /// Search for the newest matching execution under `execution_key`.
    ///
    /// ## Execution resolution algorithm
    ///
    /// 1. List all `*.json.zst` files in `manifests/<execution_key>/`.
    /// 2. Sort them **newest-first** by filename (timestamp prefix makes this correct).
    /// 3. Consider only the newest [`MAX_EXECUTIONS`] entries to bound lookup time.
    /// 4. For each execution (newest first):
    ///    - Skip if `schema_version` doesn't match [`SCHEMA_VERSION`].
    ///    - **Metadata check** (stat only): files must still exist with the same byte size.
    ///    - **Digest check** (read + fingerprint): full [`PathFingerprint`] must match.
    ///      Results are memoised in a `fingerprint_cache` shared across all executions in this
    ///      call, so paths that appear in multiple manifests are only fingerprinted once.
    ///      Fingerprinting is parallelised via rayon.
    /// 5. Return the first execution that passes both checks, or `None` if none match.
    pub(crate) fn find_matching_execution(
        &self,
        execution_key: &str,
        workspace_root: &Path,
    ) -> Result<Option<Execution>> {
        let key_dir = self.manifests_dir.join(execution_key);

        let mut executions: Vec<PathBuf> = match fs_err::read_dir(&key_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        !n.to_string_lossy().starts_with('.')
                            && n.to_string_lossy().ends_with(".json.zst")
                    })
                })
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // Newest first — filenames begin with a UTC timestamp so reverse-lexicographic
        // order is correct. Stability is irrelevant (UUID suffix makes names unique).
        executions.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
        executions.truncate(MAX_EXECUTIONS);

        // Fingerprint cache shared across executions: (path, read_dir) → PathFingerprint.
        // Keyed by both the path and the directory read flag so that a path cached as
        // Directory(None) (presence-only) does not produce a stale hit when a later
        // execution needs Directory(Some(_)) (full listing), and vice versa.
        let mut fingerprint_cache: HashMap<(PathBuf, bool), PathFingerprint> = HashMap::new();

        for path in executions {
            let compressed = match fs_err::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.log_verbose(format_args!("  skip {} (read error: {e})", path.display()));
                    continue;
                }
            };
            let bytes = match zstd::decode_all(std::io::Cursor::new(&compressed)) {
                Ok(b) => b,
                Err(e) => {
                    self.log_verbose(format_args!(
                        "  skip {} (decompress error: {e})",
                        path.display()
                    ));
                    continue;
                }
            };
            let execution: Execution = match serde_json::from_slice(&bytes) {
                Ok(c) => c,
                Err(e) => {
                    self.log_verbose(format_args!("  skip {} (parse error: {e})", path.display()));
                    continue;
                }
            };

            if execution.schema_version != SCHEMA_VERSION {
                self.log_verbose(format_args!(
                    "  skip {} (incompatible schema: was {}, now {})",
                    path.display(),
                    execution.schema_version,
                    SCHEMA_VERSION
                ));
                continue;
            }
            self.log_verbose(format_args!(
                "  checking execution {}",
                execution.id
            ));

            let meta_start = std::time::Instant::now();
            let meta_valid = self.inputs_meta_valid(&execution.inputs, workspace_root);
            self.log_verbose(format_args!(
                "    metadata check: {:.1?}",
                meta_start.elapsed()
            ));
            if !meta_valid {
                self.log_verbose(format_args!(
                    "  execution {} does not match (metadata changed)",
                    execution.id
                ));
                continue;
            }

            let digest_start = std::time::Instant::now();
            let digest_valid = self.inputs_content_valid(
                &execution.inputs,
                workspace_root,
                &mut fingerprint_cache,
            )?;
            self.log_verbose(format_args!(
                "    digest check: {:.1?}",
                digest_start.elapsed()
            ));
            if digest_valid {
                self.log_verbose(format_args!(
                    "  execution {} matched",
                    execution.id
                ));
                return Ok(Some(execution));
            }
            self.log_verbose(format_args!(
                "  execution {} does not match (content changed)",
                execution.id
            ));
        }

        Ok(None)
    }

    /// First pass: stat-only check for every input. Fast — no file reads.
    fn inputs_meta_valid(
        &self,
        inputs: &HashMap<String, PathFingerprint>,
        workspace_root: &Path,
    ) -> bool {
        for (path_str, expected) in inputs {
            let path = workspace_root.join(path_str);
            let valid = match expected {
                PathFingerprint::NotFound => matches!(
                    std::fs::metadata(&path).map_err(|e| e.kind()),
                    Err(io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
                ),
                PathFingerprint::File { size, .. } => match std::fs::metadata(&path) {
                    Ok(m) if m.is_file() && m.len() == *size => true,
                    Ok(m) if m.is_file() => {
                        self.log_verbose(format_args!(
                            "    input changed: {path_str} (size {} → {})",
                            size,
                            m.len()
                        ));
                        return false;
                    }
                    _ => false,
                },
                PathFingerprint::Directory(_) => std::fs::metadata(&path).is_ok_and(|m| m.is_dir()),
            };
            if !valid {
                self.log_verbose(format_args!(
                    "    input changed: {path_str} (metadata mismatch)"
                ));
                return false;
            }
        }
        true
    }

    /// Second pass: verify full fingerprints, reusing a shared cache across executions.
    /// Uncached inputs are fingerprinted in parallel via rayon.
    fn inputs_content_valid(
        &self,
        inputs: &HashMap<String, PathFingerprint>,
        workspace_root: &Path,
        fingerprint_cache: &mut HashMap<(PathBuf, bool), PathFingerprint>,
    ) -> Result<bool> {
        let uncached: Vec<(PathBuf, bool)> = inputs
            .iter()
            .filter_map(|(path_str, expected)| {
                let path = workspace_root.join(path_str.as_str());
                let read_dir = matches!(expected, PathFingerprint::Directory(Some(_)));
                if fingerprint_cache.contains_key(&(path.clone(), read_dir)) {
                    return None;
                }
                Some((path, read_dir))
            })
            .collect();

        let new_fingerprints: Vec<(PathBuf, bool, PathFingerprint)> = uncached
            .into_par_iter()
            .map(|(path, read_dir)| {
                let fp = fingerprint_path(&path, read_dir)?;
                Ok((path, read_dir, fp))
            })
            .collect::<Result<_>>()?;

        for (path, read_dir, fp) in new_fingerprints {
            fingerprint_cache.insert((path, read_dir), fp);
        }

        for (path_str, expected) in inputs {
            let path = workspace_root.join(path_str.as_str());
            let read_dir = matches!(expected, PathFingerprint::Directory(Some(_)));
            let Some(current) = fingerprint_cache.get(&(path, read_dir)) else {
                self.log_verbose(format_args!(
                    "    input changed: {path_str} (not fingerprinted)"
                ));
                return Ok(false);
            };
            if current != expected {
                self.log_verbose(format_args!(
                    "    input changed: {path_str} (fingerprint mismatch)"
                ));
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Store `outputs` plus captured stdout/stderr into a content-addressed archive.
    pub(crate) fn store_outputs(
        &self,
        workspace_root: &Path,
        outputs: &[PathBuf],
        stdout: &mut std::fs::File,
        stderr: &mut std::fs::File,
    ) -> Result<(String, u64)> {
        cas::store_outputs(&self.cas_dir, workspace_root, outputs, stdout, stderr)
    }

    /// Restore a previously recorded execution from the output archive.
    pub(crate) fn restore_outputs(
        &self,
        workspace_root: &Path,
        archive_key: &str,
        archive_size: u64,
    ) -> Result<()> {
        cas::restore_outputs(&self.cas_dir, workspace_root, archive_key, archive_size)
    }

    /// Serialise `execution` to `manifests/<execution_key>/<execution_id>.json`.
    ///
    /// Written directly without an atomic rename — the UUID suffix in `execution_id`
    /// prevents name collisions between concurrent writers, and partial writes are safely
    /// skipped by the JSON parser in [`find_matching_execution`](Self::find_matching_execution).
    pub(crate) fn store_execution(&self, execution_key: &str, execution: Execution) -> Result<()> {
        let key_dir = self.manifests_dir.join(execution_key);
        fs_err::create_dir_all(&key_dir)?;
        let final_path = key_dir.join(format!("{}.json.zst", execution.id));
        let json_bytes = serde_json::to_vec(&execution)?;
        let compressed = zstd::encode_all(std::io::Cursor::new(&json_bytes), 3)?;
        fs_err::write(&final_path, compressed)?;
        Ok(())
    }
}
