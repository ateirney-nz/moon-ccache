//! Content-addressed storage (CAS): tarball creation and restoration.

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::execution::Execution;

const STDOUT_ARTIFACT_NAME: &str = "__stdout__";
const STDERR_ARTIFACT_NAME: &str = "__stderr__";

/// Pack `outputs` plus captured stdout/stderr into a content-addressed gzip tarball under
/// `cas_dir`.
///
/// The tarball is written to a local temp file first (so the SHA256 hash can be computed in
/// a single pass), then copied once to `cas/<hash>.tar.gz`. Duplicate writes are safe —
/// content-addressed storage makes them idempotent.
///
/// Returns `("sha256:<hex>", compressed_size_bytes)`.
pub(super) fn create_tarball(
    cas_dir: &Path,
    workspace_root: &Path,
    outputs: &[PathBuf],
    stdout: &[u8],
    stderr: &[u8],
) -> Result<(String, u64)> {
    // Stream to a LOCAL temp file while computing the SHA256 hash.
    // Avoids writing to the (potentially gcsfuse-backed) CAS dir twice.
    let tmp_file = tempfile::NamedTempFile::new()
        .context("failed to create local temp file for tarball")?;

    let mut hashing_writer = HashingWriter {
        inner: tmp_file
            .as_file()
            .try_clone()
            .context("failed to clone temp file handle for hashing writer")?,
        hasher: Sha256::new(),
        written: 0,
    };
    {
        let enc = GzEncoder::new(&mut hashing_writer, Compression::fast());
        let mut tar = tar::Builder::new(enc);

        for path in outputs {
            let rel = path.strip_prefix(workspace_root).with_context(|| {
                format!(
                    "{} is not under workspace-root {}",
                    path.display(),
                    workspace_root.display()
                )
            })?;
            tar.append_path_with_name(path, rel)
                .with_context(|| format!("adding {} to tarball", path.display()))?;
        }

        // stdout/stderr are appended last so restore_from_cas can extract file outputs first.
        append_bytes(&mut tar, STDOUT_ARTIFACT_NAME, stdout)?;
        append_bytes(&mut tar, STDERR_ARTIFACT_NAME, stderr)?;

        tar.into_inner()?.finish()?;
    }

    let hash = hex::encode(hashing_writer.hasher.finalize());
    let size = hashing_writer.written;
    let final_path = cas_dir.join(format!("{hash}.tar.gz"));

    if !final_path.exists() {
        fs_err::copy(tmp_file.path(), &final_path)
            .with_context(|| format!("copying tarball to CAS: {}", final_path.display()))?;
    }

    Ok((format!("sha256:{hash}"), size))
}

/// Restore a previously recorded execution from the output archive referenced by `execution`.
///
/// Verifies the compressed size before decompression as a fast integrity guard.
/// File entries are extracted under `workspace_root`; `__stdout__` and `__stderr__`
/// are streamed directly to the process's stdout/stderr rather than buffered.
pub(super) fn restore_from_cas(
    cas_dir: &Path,
    execution: &Execution,
    workspace_root: &Path,
) -> Result<()> {
    let hash = execution
        .archive_key
        .strip_prefix("sha256:")
        .context("invalid archive key (expected sha256:<hex>)")?;
    let tarball = cas_dir.join(format!("{hash}.tar.gz"));
    let file = fs_err::File::open(&tarball)
        .with_context(|| format!("CAS entry not found: {}", tarball.display()))?;

    let file_size = file.metadata()?.len();
    anyhow::ensure!(
        file_size == execution.archive_size,
        "output archive corrupted: compressed size mismatch (expected {}, got {})",
        execution.archive_size,
        file_size
    );

    let mut archive = tar::Archive::new(GzDecoder::new(file));
    archive.set_overwrite(true);

    // create_tarball always appends stdout/stderr last, so file outputs are extracted first.
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if path == std::path::Path::new(STDOUT_ARTIFACT_NAME) {
            std::io::copy(&mut entry, &mut std::io::stdout())?;
        } else if path == std::path::Path::new(STDERR_ARTIFACT_NAME) {
            std::io::copy(&mut entry, &mut std::io::stderr())?;
        } else {
            entry.unpack_in(workspace_root)?;
        }
    }

    Ok(())
}

fn append_bytes<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, name, std::io::Cursor::new(data))?;
    Ok(())
}

/// An `io::Write` adapter that tees every byte through a SHA256 hasher while forwarding
/// writes to an inner sink. Used by [`create_tarball`] to compute the CAS key in a single
/// streaming pass without re-reading the temp file after writing.
struct HashingWriter<W: std::io::Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: std::io::Write> std::io::Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
