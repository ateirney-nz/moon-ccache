//! Input filter built from CLI `--exclude` flags.

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};

/// Glob-based filter that drops fspy-observed reads matching any `--exclude` pattern.
///
/// A path is accepted when it is not matched by any exclude pattern.
/// Excludes come from `--exclude` CLI flags (project-relative unless they start with `/`).
///
/// When no excludes are provided, `PathFilter::new` returns `None` and all reads pass
/// through unfiltered.
#[derive(Clone)]
pub(crate) struct PathFilter {
    excludes: globset::GlobSet,
}

impl PathFilter {
    /// Build an exclude-only filter from CLI `--exclude` patterns.
    ///
    /// Patterns follow the Moon convention: leading `/` = workspace-root relative,
    /// no leading `/` = project-relative (prepend `project_prefix`).
    ///
    /// Returns `None` when `cli_excludes` is empty.
    pub(crate) fn new(cli_excludes: &[String], project_prefix: &str) -> Result<Option<Self>> {
        if cli_excludes.is_empty() {
            return Ok(None);
        }

        let mut builder = GlobSetBuilder::new();
        for pat in cli_excludes {
            let resolved = Self::resolve_cli_pattern(pat, project_prefix);
            builder.add(
                Glob::new(&resolved)
                    .with_context(|| format!("invalid --exclude pattern: {pat}"))?,
            );
        }

        Ok(Some(Self {
            excludes: builder
                .build()
                .context("failed to compile exclude patterns")?,
        }))
    }

    /// Normalise a CLI-supplied `--exclude` pattern to a workspace-relative glob.
    fn resolve_cli_pattern(bare: &str, project_prefix: &str) -> String {
        if let Some(ws_rel) = bare.strip_prefix('/') {
            ws_rel.to_string()
        } else if project_prefix.is_empty() {
            bare.to_string()
        } else {
            format!("{project_prefix}/{bare}")
        }
    }

    /// Returns `true` if the workspace-relative path `rel` should be tracked as an input
    /// (i.e. it is not matched by any exclude pattern).
    pub(crate) fn allows(&self, rel: &str) -> bool {
        !self.excludes.is_match(rel)
    }
}
