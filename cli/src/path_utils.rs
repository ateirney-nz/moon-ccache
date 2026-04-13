//! Path utilities shared across ccache modules.

use std::path::Path;

/// Convert `path` to a workspace-relative string.
/// Returns the path unchanged (as a lossy string) when it is not under `workspace_root`.
pub(crate) fn to_relative_path(path: &Path, workspace_root: &Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
