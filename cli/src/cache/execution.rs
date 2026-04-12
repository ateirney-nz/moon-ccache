//! Execution record.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::TraceManifest;

use super::SCHEMA_VERSION;

/// A stored record of a prior command execution: the trace manifest that was fingerprinted
/// and the archive key for the output archive.
///
/// The `execution_id` doubles as the on-disk filename (`{execution_id}.json`). Because the
/// timestamp is the leading component, a reverse-lexicographic sort of the directory listing
/// automatically yields newest-first order, which [`super::Cache::find_matching_execution`]
/// exploits to prefer the most recently recorded execution.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Execution {
    /// The `MOON_TARGET` value this execution was recorded for (e.g. `my-pkg:build`).
    pub(crate) target: String,
    /// Unique identifier with format `YYYYMMDD-HHMMSS.mmm-<8-char-uuid>`.
    pub(crate) execution_id: String,
    /// RFC 3339 wall-clock time this execution was recorded.
    pub(crate) created_at: String,
    /// The exact command line that produced this execution.
    pub(crate) command: Vec<String>,
    /// Input fingerprints used to validate this execution on lookup.
    pub(crate) manifest: TraceManifest,
    /// Content-addressed storage key for the output archive: `sha256:<hex>`.
    pub(crate) archive_key: String,
    /// Compressed size of the output archive in bytes.
    pub(crate) archive_size: u64,
    /// Schema version bump required when the execution format changes in a breaking way.
    pub(crate) schema_version: u32,
}

impl Execution {
    /// Construct an [`Execution`] ready to be written to the execution store.
    ///
    /// The `execution_id` has the form `YYYYMMDD-HHMMSS.mmm-<8-char-uuid>`:
    /// - The **timestamp prefix** ensures reverse-lexicographic sort yields newest-first order.
    /// - The **UUID suffix** makes concurrent writers collision-safe without a lock.
    pub(crate) fn new(
        target: &str,
        command: &[String],
        manifest: TraceManifest,
        archive_key: &str,
        archive_size: u64,
    ) -> Self {
        let now = Utc::now();
        let uuid = Uuid::new_v4().to_string();
        let uuid_suffix = &uuid[..8];
        Self {
            target: target.to_string(),
            execution_id: format!("{}-{uuid_suffix}", now.format("%Y%m%d-%H%M%S%.3f")),
            created_at: now.to_rfc3339(),
            command: command.to_vec(),
            manifest,
            archive_key: archive_key.to_string(),
            archive_size,
            schema_version: SCHEMA_VERSION,
        }
    }
}
