//! Task information loaded from the Moon project snapshot.

use anyhow::{Context, Result};
use moon_project::Project;
use moon_task::Task;
use std::path::{Path, PathBuf};

use crate::moon_err;

pub(crate) struct TaskInfo {
    /// Concrete input files returned by `Task::get_input_files`.
    pub(crate) input_files: Vec<PathBuf>,
    /// The resolved task, kept so `output_files()` can be called after the task runs.
    task: Task,
    workspace_root: PathBuf,
}

impl TaskInfo {
    /// Return the declared output files that exist on disk.
    ///
    /// Must be called **after** the wrapped task has run so that output files are present.
    pub(crate) fn output_files(&self) -> Result<Vec<PathBuf>> {
        self.task
            .get_output_files(&self.workspace_root, true)
            .map_err(moon_err)
    }
}

/// Load task info from the Moon project snapshot.
///
/// Looks up the task for `target` in the snapshot, errors if not found.
pub(crate) fn load_task_info(
    target: &str,
    snapshot_path: &Path,
    workspace_root: &Path,
) -> Result<TaskInfo> {
    let task_name = target.split(':').next_back().unwrap_or(target);

    let data = fs_err::read(snapshot_path)
        .with_context(|| format!("reading snapshot: {}", snapshot_path.display()))?;
    let snapshot: Project =
        serde_json::from_slice(&data).context("parsing MOON_PROJECT_SNAPSHOT")?;

    let task = snapshot.get_task(task_name).map_err(moon_err)?.clone();

    let input_files = task.get_input_files(workspace_root).map_err(moon_err)?;

    Ok(TaskInfo {
        input_files,
        task,
        workspace_root: workspace_root.to_path_buf(),
    })
}
