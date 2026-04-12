//! Execution store, content-addressed storage (CAS), and declared cache key computation.

mod cas;
mod execution;
mod key;

pub(crate) use execution::Execution;
pub(crate) use key::declared_cache_key;

use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::fingerprint::{PathFingerprint, PathRead, fingerprint_path};
use crate::manifest::TraceManifest;

pub(crate) const MAX_EXECUTIONS: usize = 10;
pub(crate) const SCHEMA_VERSION: u32 = 1;

const JSON_EXTENSION: &str = "json";

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

    fn vlog(&self, args: std::fmt::Arguments<'_>) {
        if self.verbose {
            eprintln!("{args}");
        }
    }

    /// Search for the newest matching execution under `declared_key`.
    ///
    /// ## Execution resolution algorithm
    ///
    /// 1. List all `*.json` files in `manifests/<declared_key>/`.
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
        declared_key: &str,
        workspace_root: &Path,
    ) -> Result<Option<Execution>> {
        let key_dir = self.manifests_dir.join(declared_key);

        let mut executions: Vec<PathBuf> = match fs_err::read_dir(&key_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == JSON_EXTENSION)
                        && !p
                            .file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                })
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // Newest first — filenames begin with a UTC timestamp so reverse-lexicographic
        // order is correct. Stability is irrelevant (UUID suffix makes names unique).
        executions.sort_unstable_by(|a, b| b.file_name().cmp(&a.file_name()));
        executions.truncate(MAX_EXECUTIONS);

        // Fingerprint cache shared across executions: (path, PathRead) → PathFingerprint.
        // Keyed by both the path and the access detail level so that a path cached as
        // Directory(None) (presence-only) does not produce a stale hit when a later
        // execution needs Directory(Some(_)) (full listing), and vice versa.
        let mut fingerprint_cache: HashMap<(PathBuf, PathRead), PathFingerprint> =
            HashMap::new();

        for path in executions {
            let bytes = match fs_err::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.vlog(format_args!("  skip {} (read error: {e})", path.display()));
                    continue;
                }
            };
            let execution: Execution = match serde_json::from_slice(&bytes) {
                Ok(c) => c,
                Err(e) => {
                    self.vlog(format_args!("  skip {} (parse error: {e})", path.display()));
                    continue;
                }
            };

            if execution.schema_version != SCHEMA_VERSION {
                self.vlog(format_args!(
                    "  skip {} (incompatible schema: was {}, now {})",
                    path.display(),
                    execution.schema_version,
                    SCHEMA_VERSION
                ));
                continue;
            }
            self.vlog(format_args!(
                "  checking execution {}",
                execution.execution_id
            ));

            let meta_start = std::time::Instant::now();
            let meta_valid = self.inputs_meta_valid(&execution.manifest, workspace_root);
            self.vlog(format_args!(
                "    metadata check: {:.1?}",
                meta_start.elapsed()
            ));
            if !meta_valid {
                self.vlog(format_args!(
                    "  execution {} does not match (metadata changed)",
                    execution.execution_id
                ));
                continue;
            }

            let digest_start = std::time::Instant::now();
            let digest_valid = self.inputs_content_valid(
                &execution.manifest,
                workspace_root,
                &mut fingerprint_cache,
            )?;
            self.vlog(format_args!(
                "    digest check: {:.1?}",
                digest_start.elapsed()
            ));
            if digest_valid {
                self.vlog(format_args!(
                    "  execution {} matched",
                    execution.execution_id
                ));
                return Ok(Some(execution));
            }
            self.vlog(format_args!(
                "  execution {} does not match (content changed)",
                execution.execution_id
            ));
        }

        Ok(None)
    }

    /// First pass: stat-only check for every input. Fast — no file reads.
    fn inputs_meta_valid(&self, manifest: &TraceManifest, workspace_root: &Path) -> bool {
        for (path_str, expected) in &manifest.inputs {
            let path = workspace_root.join(path_str);
            let valid = match expected {
                PathFingerprint::NotFound => matches!(
                    std::fs::metadata(&path).map_err(|e| e.kind()),
                    Err(io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
                ),
                PathFingerprint::File { size, .. } => match std::fs::metadata(&path) {
                    Ok(m) if m.is_file() && m.len() == *size => true,
                    Ok(m) if m.is_file() => {
                        self.vlog(format_args!(
                            "    input changed: {path_str} (size {} → {})",
                            size,
                            m.len()
                        ));
                        return false;
                    }
                    _ => false,
                },
                PathFingerprint::Directory(_) => {
                    std::fs::metadata(&path).is_ok_and(|m| m.is_dir())
                }
            };
            if !valid {
                self.vlog(format_args!(
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
        manifest: &TraceManifest,
        workspace_root: &Path,
        fingerprint_cache: &mut HashMap<(PathBuf, PathRead), PathFingerprint>,
    ) -> Result<bool> {
        let uncached: Vec<(PathBuf, PathRead)> = manifest
            .inputs
            .iter()
            .filter_map(|(path_str, expected)| {
                let path = workspace_root.join(path_str.as_str());
                let path_read = PathRead {
                    read_dir_entries: matches!(expected, PathFingerprint::Directory(Some(_))),
                };
                if fingerprint_cache.contains_key(&(path.clone(), path_read)) {
                    return None;
                }
                Some((path, path_read))
            })
            .collect();

        let new_fingerprints: Vec<(PathBuf, PathRead, PathFingerprint)> = uncached
            .into_par_iter()
            .map(|(path, path_read)| {
                let fp = fingerprint_path(&path, path_read)?;
                Ok((path, path_read, fp))
            })
            .collect::<Result<_>>()?;

        for (path, path_read, fp) in new_fingerprints {
            fingerprint_cache.insert((path, path_read), fp);
        }

        for (path_str, expected) in &manifest.inputs {
            let path = workspace_root.join(path_str.as_str());
            let path_read = PathRead {
                read_dir_entries: matches!(expected, PathFingerprint::Directory(Some(_))),
            };
            let Some(current) = fingerprint_cache.get(&(path, path_read)) else {
                self.vlog(format_args!(
                    "    input changed: {path_str} (not fingerprinted)"
                ));
                return Ok(false);
            };
            if current != expected {
                self.vlog(format_args!(
                    "    input changed: {path_str} (fingerprint mismatch)"
                ));
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Pack `outputs` plus captured stdout/stderr into a content-addressed gzip tarball.
    pub(crate) fn create_tarball(
        &self,
        workspace_root: &Path,
        outputs: &[PathBuf],
        stdout: &[u8],
        stderr: &[u8],
    ) -> Result<(String, u64)> {
        cas::create_tarball(&self.cas_dir, workspace_root, outputs, stdout, stderr)
    }

    /// Restore a previously recorded execution from the output archive referenced by `execution`.
    pub(crate) fn restore_from_cas(
        &self,
        execution: &Execution,
        workspace_root: &Path,
    ) -> Result<()> {
        cas::restore_from_cas(&self.cas_dir, execution, workspace_root)
    }

    /// Serialise `execution` to `manifests/<declared_key>/<execution_id>.json`.
    ///
    /// Written directly without an atomic rename — the UUID suffix in `execution_id`
    /// prevents name collisions between concurrent writers, and partial writes are safely
    /// skipped by the JSON parser in [`find_matching_execution`](Self::find_matching_execution).
    pub(crate) fn record_execution(&self, declared_key: &str, execution: Execution) -> Result<()> {
        let key_dir = self.manifests_dir.join(declared_key);
        fs_err::create_dir_all(&key_dir)?;
        let final_path = key_dir.join(format!("{}.json", execution.execution_id));
        fs_err::write(&final_path, serde_json::to_vec(&execution)?)?;
        Ok(())
    }
}
