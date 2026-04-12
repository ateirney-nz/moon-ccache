//! fspy-based command tracing: spawn a command under fspy and collect its raw file accesses.
//!
//! This module is intentionally narrow — it only handles process lifecycle (spawn, signal
//! forwarding, stdout/stderr capture) and access collection.  Interpreting which accesses
//! are inputs, filtering them, and fingerprinting is the caller's responsibility.
//! 
//! Consideration should be made to whether change this to be inspired by the likes of https://github.com/voidzero-dev/vite-task/blob/076cef486127e6cd1fefc58945f00dac316888ca/crates/vite_task/src/session/execute/spawn.rs#L87

use anyhow::{Context, Result};
use fspy::AccessMode;
use fspy_shared::ipc::NativePath;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Maximum bytes to capture from stdout/stderr into the cache archive.
/// Beyond this, output is still forwarded to the terminal but not stored.
const MAX_CAPTURED_OUTPUT: usize = 64 * 1024 * 1024; // 64 MiB

pub(crate) struct TraceResult {
    /// Every file-system access observed by fspy during the run, in report order.
    /// Each entry is `(mode, absolute_path)`.  Both reads and writes are included;
    /// callers decide which to treat as inputs, outputs, or irrelevant.
    pub(crate) path_accesses: Vec<(AccessMode, PathBuf)>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Spawn `command` under fspy, forwarding stdout/stderr to the terminal while capturing them.
///
/// Returns a [`TraceResult`] containing every file-system access fspy observed, plus the
/// captured streams.  The caller is responsible for interpreting the accesses (separating
/// reads from writes, filtering, fingerprinting).
///
/// Exits the process with the command's exit code if the command fails.
pub(crate) async fn trace_and_run(command: &[String]) -> Result<TraceResult> {
    use tokio::signal::unix::{SignalKind, signal};

    let token = CancellationToken::new();

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let signal_token = token.clone();
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        // Cancelling the token is how fspy's spawn implementation terminates the
        // traced child process — no explicit kill is needed here.
        signal_token.cancel();
    });

    let mut cmd = fspy::Command::new(&command[0]);
    cmd.envs(std::env::vars_os())
        .args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn(token)
        .await
        .context("failed to spawn command under fspy")?;

    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        match child_stdout {
            Some(r) => tee(r, tokio::io::stdout(), "stdout").await,
            None => Ok(Vec::new()),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match child_stderr {
            Some(r) => tee(r, tokio::io::stderr(), "stderr").await,
            None => Ok(Vec::new()),
        }
    });

    let termination = child
        .wait_handle
        .await
        .context("failed to wait for traced command")?;
    signal_task.abort();
    let stdout = stdout_task.await.context("stdout task panicked")??;
    let stderr = stderr_task.await.context("stderr task panicked")??;

    if !termination.status.success() {
        std::process::exit(termination.status.code().unwrap_or(1));
    }

    let path_accesses = termination
        .path_accesses
        .iter()
        .map(|access| (access.mode, native_path_to_pathbuf(access.path)))
        .collect();

    Ok(TraceResult {
        path_accesses,
        stdout,
        stderr,
    })
}

/// Forward `reader` to `writer` (the terminal) while capturing up to `MAX_CAPTURED_OUTPUT`
/// bytes for the cache archive. Warns if output is truncated.
async fn tee<R, W>(
    mut reader: R,
    mut writer: W,
    stream_name: &'static str,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if buf.len() < MAX_CAPTURED_OUTPUT {
            let remaining = MAX_CAPTURED_OUTPUT - buf.len();
            buf.extend_from_slice(&chunk[..n.min(remaining)]);
            if n > remaining {
                truncated = true;
            }
        } else if !truncated {
            truncated = true;
        }
        writer.write_all(&chunk[..n]).await?;
    }
    if truncated {
        eprintln!(
            "ccache: {stream_name} exceeded {} MiB capture limit — cached output will be \
             truncated on restore",
            MAX_CAPTURED_OUTPUT / 1024 / 1024
        );
    }
    Ok(buf)
}

fn native_path_to_pathbuf(path: &NativePath) -> PathBuf {
    let p = path.strip_path_prefix("", |r| r.map(Path::to_path_buf).unwrap_or_default());
    if p.as_os_str().is_empty() {
        panic!("fspy returned an empty path — this is a bug in the fspy integration");
    }
    p
}
