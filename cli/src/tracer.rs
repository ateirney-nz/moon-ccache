//! fspy-based command tracing: spawn a command under fspy and collect its file accesses.
//!
//! Modelled after `spawn_with_tracking` from the vite_task crate:
//! <https://github.com/voidzero-dev/vite-task/blob/main/crates/vite_task/src/session/execute/spawn.rs>

use anyhow::{Context, Result};
use fspy::AccessMode;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

/// Describes how a path was accessed during the traced run, as reported by fspy.
///
/// Carried in [`TraceResult::path_reads`] and used by callers to decide the level of
/// detail to capture when fingerprinting directory paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TracePathRead {
    /// Whether the process called `readdir`/`getdents` on this path (fspy `READ_DIR` flag).
    /// When `false` the directory was only opened (existence check), not listed.
    pub(crate) read_dir_entries: bool,
}

/// The result of a traced command execution.
pub(crate) struct TraceResult {
    /// Workspace-relative paths that were read during the run, with access metadata.
    /// A path may also appear in `path_writes`; callers decide whether to exclude those.
    pub(crate) path_reads: HashMap<String, TracePathRead>,

    /// Workspace-relative paths that were written during the run.
    /// Callers use this to exclude self-generated files from the input fingerprint.
    pub(crate) path_writes: HashSet<String>,

    /// Captured stdout, spooled to a temporary file.
    /// Deleted automatically when this struct is dropped.
    pub(crate) stdout: NamedTempFile,

    /// Captured stderr, spooled to a temporary file.
    /// Deleted automatically when this struct is dropped.
    pub(crate) stderr: NamedTempFile,

    /// The command's exit status.
    pub(crate) exit_status: ExitStatus,
}

/// Spawn `command` under fspy, forwarding stdout/stderr to the terminal while capturing them.
///
/// Path accesses are normalised to workspace-relative strings in this function:
/// paths outside `workspace_root` are discarded, `..` components are resolved, and
/// `.git` accesses are skipped. Reads and writes are separated into distinct collections.
///
/// Returns a [`TraceResult`] with the exit status; the caller is responsible for
/// deciding whether to exit the process on failure.
pub(crate) async fn trace_and_run(
    command: &[String],
    workspace_root: &Path,
) -> Result<TraceResult> {
    use tokio::signal::unix::{SignalKind, signal};

    let token = CancellationToken::new();

    let mut sigint = signal(SignalKind::interrupt()).context("failed to install SIGINT handler")?;
    let mut sigterm =
        signal(SignalKind::terminate()).context("failed to install SIGTERM handler")?;
    let signal_token = token.clone();
    tokio::spawn(async move {
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn(token.clone())
        .await
        .context("failed to spawn command under fspy")?;

    let mut child_stdout = child.stdout.take().expect("stdout was piped");
    let mut child_stderr = child.stderr.take().expect("stderr was piped");

    // Read stdout and stderr concurrently, forwarding to the terminal and spooling to
    // temp files for later archiving into the CAS tarball.
    // Using tokio::select! prevents deadlock when the process writes large amounts to
    // both streams simultaneously — a sequential read on one stream would block while
    // the other fills its pipe buffer and stalls the child.
    let mut stdout_file = NamedTempFile::new().context("failed to create stdout temp file")?;
    let mut stderr_file = NamedTempFile::new().context("failed to create stderr temp file")?;
    let mut stdout_chunk = [0u8; 8192];
    let mut stderr_chunk = [0u8; 8192];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut term_stdout = tokio::io::stdout();
    let mut term_stderr = tokio::io::stderr();

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            result = child_stdout.read(&mut stdout_chunk), if !stdout_done => {
                match result.context("error reading child stdout")? {
                    0 => stdout_done = true,
                    n => {
                        term_stdout.write_all(&stdout_chunk[..n]).await
                            .context("error writing to stdout")?;
                        stdout_file.write_all(&stdout_chunk[..n])
                            .context("error writing to stdout temp file")?;
                    }
                }
            }
            result = child_stderr.read(&mut stderr_chunk), if !stderr_done => {
                match result.context("error reading child stderr")? {
                    0 => stderr_done = true,
                    n => {
                        term_stderr.write_all(&stderr_chunk[..n]).await
                            .context("error writing to stderr")?;
                        stderr_file.write_all(&stderr_chunk[..n])
                            .context("error writing to stderr temp file")?;
                    }
                }
            }
            () = token.cancelled() => break,
        }
    }

    let termination = child
        .wait_handle
        .await
        .context("failed to wait for traced command")?;

    // Process fspy path accesses: normalise to workspace-relative strings, skip paths
    // outside the workspace root and under `.git`, and separate reads from writes.
    let mut path_reads: HashMap<String, TracePathRead> = HashMap::new();
    let mut path_writes: HashSet<String> = HashSet::new();

    for access in termination.path_accesses.iter() {
        let Some(rel) = access.path.strip_path_prefix(workspace_root, |result| {
            let Ok(stripped) = result else {
                return None;
            };
            let rel = normalize_relative(stripped);
            if rel.is_empty() {
                return None;
            }
            if rel == ".git" || rel.starts_with(".git/") {
                return None;
            }
            Some(rel)
        }) else {
            continue;
        };

        if access.mode.contains(AccessMode::WRITE) {
            path_writes.insert(rel.clone());
        }
        if access
            .mode
            .intersects(AccessMode::READ | AccessMode::READ_DIR)
        {
            let read_dir_entries = access.mode.contains(AccessMode::READ_DIR);
            path_reads
                .entry(rel)
                .and_modify(|pr| pr.read_dir_entries |= read_dir_entries)
                .or_insert(TracePathRead { read_dir_entries });
        }
    }

    Ok(TraceResult {
        path_reads,
        path_writes,
        stdout: stdout_file,
        stderr: stderr_file,
        exit_status: termination.status,
    })
}

/// Normalise a relative path by resolving `..` components.
///
/// Equivalent to `RelativePathBuf::clean()` in the vite_path crate.
fn normalize_relative(path: &Path) -> String {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(c) => parts.push(c),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    parts
        .iter()
        .map(|c| c.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
