//! Task information loaded from the Moon project snapshot.

use anyhow::{Context, Result};
use moon_project::Project;
use moon_task::Task;
use std::path::{Path, PathBuf};

use crate::moon_err;

pub(crate) struct TaskInfo {
    /// Concrete input files returned by `Task::get_input_files`.
    pub(crate) input_files: Vec<PathBuf>,
    /// Task-declared environment variables (`env` in moon.yml).
    /// Included in the execution key so that changing a hardcoded env value invalidates the cache.
    /// Values are `Option<String>` because Moon allows unsetting a variable with a null value.
    pub(crate) env: std::collections::BTreeMap<String, Option<String>>,
    /// Environment variable names declared as task inputs (`input_env` in moon.yml).
    /// Their runtime values are resolved from the process environment and included in the key.
    pub(crate) input_env: Vec<String>,
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
/// Looks up `task_name` (e.g. `build`) in the snapshot, errors if not found.
pub(crate) fn load_task_info(
    task_name: &str,
    snapshot_path: &Path,
    workspace_root: &Path,
) -> Result<TaskInfo> {
    let data = fs_err::read(snapshot_path)
        .with_context(|| format!("reading snapshot: {}", snapshot_path.display()))?;
    let snapshot: Project =
        serde_json::from_slice(&data).context("parsing MOON_PROJECT_SNAPSHOT")?;

    let task = snapshot.get_task(task_name).map_err(moon_err)?.clone();

    let input_files = task.get_input_files(workspace_root).map_err(moon_err)?;
    let env = task.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut input_env: Vec<String> = task.input_env.iter().cloned().collect();
    input_env.sort();

    Ok(TaskInfo {
        input_files,
        env,
        input_env,
        task,
        workspace_root: workspace_root.to_path_buf(),
    })
}
