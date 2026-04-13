//! Content-addressed storage (CAS): tarball creation and restoration.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const STDOUT_ARTIFACT_NAME: &str = "__stdout__";
const STDERR_ARTIFACT_NAME: &str = "__stderr__";

/// Pack `outputs` plus captured stdout/stderr into a content-addressed zstd tarball under
/// `cas_dir`.
///
/// The tarball is written to a local temp file first (so the SHA256 hash can be computed in
/// a single pass), then copied once to `cas/<hash>.tar.zst`. Duplicate writes are safe —
/// content-addressed storage makes them idempotent.
///
/// Returns `("sha256:<hex>", compressed_size_bytes)`.
pub(super) fn store_outputs(
    cas_dir: &Path,
    workspace_root: &Path,
    outputs: &[PathBuf],
    stdout: &mut std::fs::File,
    stderr: &mut std::fs::File,
) -> Result<(String, u64)> {
    // Stream to a LOCAL temp file while computing the SHA256 hash.
    // Avoids writing to the (potentially gcsfuse-backed) CAS dir twice.
    let tmp_file =
        tempfile::NamedTempFile::new().context("failed to create local temp file for tarball")?;

    let mut hashing_writer = HashingWriter {
        inner: tmp_file
            .as_file()
            .try_clone()
            .context("failed to clone temp file handle for hashing writer")?,
        hasher: Sha256::new(),
        written: 0,
    };
    {
        let enc =
            zstd::Encoder::new(&mut hashing_writer, 3).context("failed to create zstd encoder")?;
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

        // stdout/stderr are appended last so restore_outputs can extract file outputs first.
        append_file(&mut tar, STDOUT_ARTIFACT_NAME, stdout)?;
        append_file(&mut tar, STDERR_ARTIFACT_NAME, stderr)?;

        tar.into_inner()?.finish()?;
    }

    let hash = hex::encode(hashing_writer.hasher.finalize());
    let size = hashing_writer.written;
    let final_path = cas_dir.join(format!("{hash}.tar.zst"));

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
pub(super) fn restore_outputs(
    cas_dir: &Path,
    workspace_root: &Path,
    archive_key: &str,
    archive_size: u64,
) -> Result<()> {
    let hash = archive_key
        .strip_prefix("sha256:")
        .context("invalid archive key (expected sha256:<hex>)")?;
    let tarball = cas_dir.join(format!("{hash}.tar.zst"));
    let file = fs_err::File::open(&tarball)
        .with_context(|| format!("CAS entry not found: {}", tarball.display()))?;

    let file_size = file.metadata()?.len();
    anyhow::ensure!(
        file_size == archive_size,
        "output archive corrupted: compressed size mismatch (expected {}, got {})",
        archive_size,
        file_size
    );

    let dec = zstd::Decoder::new(file).context("failed to create zstd decoder")?;
    let mut archive = tar::Archive::new(dec);
    archive.set_overwrite(true);

    // store_outputs always appends stdout/stderr last, so file outputs are extracted first.
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

fn append_file<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    file: &mut std::fs::File,
) -> Result<()> {
    use std::io::Seek as _;
    file.seek(std::io::SeekFrom::Start(0))
        .context("failed to seek stdout/stderr temp file")?;
    let size = file
        .metadata()
        .context("failed to stat stdout/stderr temp file")?
        .len();
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, name, file)?;
    Ok(())
}

/// An `io::Write` adapter that tees every byte through a SHA256 hasher while forwarding
/// writes to an inner sink. Used by [`store_outputs`] to compute the CAS key in a single
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
