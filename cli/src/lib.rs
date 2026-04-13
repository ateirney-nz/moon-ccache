//! Moon build command cache.
//!
//! See the project README for full documentation.

mod cache;
mod path_filter;
mod path_fingerprint;
mod path_utils;
mod task_info;
mod tracer;

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use cache::{Cache, Execution, execution_key};
use path_filter::PathFilter;
use path_fingerprint::{PathFingerprint, fingerprint_path};
use path_utils::to_relative_path;
use task_info::load_task_info;
use tracer::trace_and_run;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Args {
    /// Print cache hit/miss and execution validation details.
    #[arg(long)]
    verbose: bool,

    /// Report paths read/written during execution that are not declared in moon inputs/outputs.
    /// Useful for identifying gaps in task configuration, but may produce false positives
    /// for tool-specific dependencies (node_modules, .venv, etc.) that Moon tracks separately.
    #[arg(long)]
    report_undeclared: bool,

    /// Exclude fspy-observed reads matching this glob from the input fingerprint.
    /// All workspace reads are tracked; only paths matching an --exclude pattern
    /// are dropped from the fingerprint.
    /// Can be repeated.
    ///
    /// Patterns with a leading `/` are workspace-root relative; all others are
    /// relative to the package directory ($MOON_PROJECT_ROOT).
    ///
    /// Examples:
    ///   --exclude '**/node_modules/**' --exclude '/pnpm-lock.yaml'
    #[arg(long = "exclude", value_name = "PATTERN")]
    exclude_inputs: Vec<String>,

    /// The command to run and cache (everything after --).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Run the ccache binary: look up a matching execution, restore it, or trace and record on miss.
pub async fn run() -> Result<()> {
    let mut args = Args::parse();

    anyhow::ensure!(!args.command.is_empty(), "no command specified after --");

    if !args.verbose && env::var("MOON_CCACHE_VERBOSE").as_deref() == Ok("true") {
        args.verbose = true;
    }

    let target =
        env::var("MOON_TARGET").context("MOON_TARGET is not set — ccache must be run via Moon")?;
    let task_name = env::var("MOON_TASK_ID")
        .context("MOON_TASK_ID is not set — ccache must be run via Moon")?;

    let cache_dir = resolve_env_path("MOON_CACHE_DIR")?.join("ccache");
    let project_root = resolve_env_path("MOON_PROJECT_ROOT")?;
    let snapshot_path = resolve_env_path("MOON_PROJECT_SNAPSHOT")?;
    let workspace_root = resolve_env_path("MOON_WORKSPACE_ROOT")?;

    // The project prefix is MOON_PROJECT_ROOT as a workspace-relative path.
    // Used to resolve project-relative --exclude patterns.
    // Empty when the project root equals the workspace root.
    let project_prefix = {
        let p = to_relative_path(&project_root, &workspace_root)
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        if p.is_empty() || p == "." {
            String::new()
        } else {
            p
        }
    };

    let task_info = load_task_info(&task_name, &snapshot_path, &workspace_root)?;

    if args.verbose {
        eprintln!(
            "ccache: snapshot: {} input file(s) (project: {})",
            task_info.input_files.len(),
            if project_prefix.is_empty() {
                "<workspace root>"
            } else {
                &project_prefix
            }
        );
    }

    let input_filter = PathFilter::new(&args.exclude_inputs, &project_prefix)?;
    let report_exclusions = if args.report_undeclared {
        Some(build_report_exclusions()?)
    } else {
        None
    };
    let is_report_excluded = |rel: &str| -> bool {
        report_exclusions.as_ref().is_some_and(|f| f.is_match(rel))
    };
    if args.verbose {
        if input_filter.is_none() {
            eprintln!("ccache: input tracking: all workspace reads");
        } else {
            eprintln!(
                "ccache: input tracking: {} exclude(s) from CLI",
                args.exclude_inputs.len(),
            );
        }
    }

    let cache = Cache::new(&cache_dir, args.verbose)?;

    let execution_key = execution_key(
        &target,
        &args.command,
        &task_info,
        &workspace_root,
        &args.exclude_inputs,
    )?;
    if args.verbose {
        eprintln!(
            "ccache: execution key: {execution_key} (target={target}, command={:?})",
            args.command
        );
    }

    let lookup_start = std::time::Instant::now();
    let hit = {
        let execution_key = execution_key.clone();
        let workspace_root = workspace_root.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            cache.find_matching_execution(&execution_key, &workspace_root)
        })
        .await
        .context("find_matching_execution panicked")??
    };
    if let Some(execution) = hit {
        if args.verbose {
            eprintln!("cache hit {target} ({:.1?})", lookup_start.elapsed());
        }
        let workspace_root = workspace_root.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            cache.restore_outputs(
                &workspace_root,
                &execution.archive_key,
                execution.archive_size,
            )
        })
        .await
        .context("restore_outputs panicked")??;
        return Ok(());
    }

    if args.verbose {
        eprintln!("cache miss {target} ({:.1?})", lookup_start.elapsed());
    }

    let tracer::TraceResult {
        path_reads,
        path_writes,
        mut stdout,
        mut stderr,
        exit_status,
    } = trace_and_run(&args.command, &workspace_root).await?;

    if !exit_status.success() {
        std::process::exit(exit_status.code().unwrap_or(1));
    }

    let inputs: HashMap<String, PathFingerprint> = {
        let workspace_root = workspace_root.clone();
        let filter = input_filter.clone();
        let path_reads_for_fp = path_reads.clone();
        let path_writes_for_fp = path_writes.clone();
        tokio::task::spawn_blocking(move || {
            path_reads_for_fp
                .par_iter()
                .filter(|(rel, _)| !path_writes_for_fp.contains(*rel))
                .filter(|(rel, _)| filter.as_ref().is_none_or(|f| f.allows(rel)))
                .map(|(rel, path_read)| {
                    let fp = fingerprint_path(&workspace_root.join(rel), path_read.read_dir_entries)?;
                    Ok((rel.clone(), fp))
                })
                .collect::<Result<_>>()
        })
        .await
        .context("input fingerprinting panicked")??
    };
    if args.verbose {
        eprintln!("ccache: traced inputs: {}", inputs.len());
    }

    let output_paths = task_info.output_files()?;
    if args.verbose {
        eprintln!("ccache: output files: {}", output_paths.len());
    }
    let declared_outputs: std::collections::HashSet<String> = output_paths
        .iter()
        .map(|p| to_relative_path(p, &workspace_root))
        .collect();

    // Slash-terminated project prefix used to strip workspace-relative paths down to
    // project-relative form when printing --report-undeclared warnings.
    let project_prefix_slash = if project_prefix.is_empty() {
        String::new()
    } else {
        format!("{project_prefix}/")
    };

    // Warn about paths that were exclusively read within the project but are not declared
    // as moon inputs. Better input coverage improves moon's own execution targeting.
    if args.report_undeclared {
        let declared_inputs: std::collections::HashSet<String> = task_info
            .input_files
            .iter()
            .map(|p| to_relative_path(p, &workspace_root))
            .collect();

        let project_in_scope = |rel: &str| -> bool {
            if project_prefix.is_empty() {
                true
            } else {
                rel == project_prefix || rel.starts_with(&format!("{project_prefix}/"))
            }
        };

        let mut undeclared: Vec<&str> = path_reads
            .keys()
            .filter(|rel| !path_writes.contains(*rel))
            .filter(|rel| project_in_scope(rel))
            .filter(|rel| !declared_inputs.contains(*rel))
            .filter(|rel| !declared_outputs.contains(*rel))
            .filter(|rel| !is_report_excluded(rel))
            .filter(|rel| workspace_root.join(rel).is_file())
            .map(String::as_str)
            .collect();

        if !undeclared.is_empty() {
            undeclared.sort_unstable();
            eprintln!(
                "ccache: warning: {} path(s) read within project but not declared as moon inputs:",
                undeclared.len()
            );
            for rel in undeclared {
                eprintln!("  {}", project_display(rel, &project_prefix_slash));
            }
        }

        // Warn about declared inputs that were written to during execution.
        // A declared input being modified by its own task suggests misconfiguration:
        // the path should likely also be declared as an output, or the task should not
        // be writing to it. Moon will re-invalidate the task on the next run because the
        // input hash will have changed.
        let mut written_inputs: Vec<&str> = declared_inputs
            .iter()
            .filter(|rel| path_writes.contains(*rel))
            .map(String::as_str)
            .collect();

        if !written_inputs.is_empty() {
            written_inputs.sort_unstable();
            eprintln!(
                "ccache: warning: {} declared input(s) written to during execution:",
                written_inputs.len()
            );
            for rel in written_inputs {
                if project_in_scope(rel) {
                    eprintln!("  {}", project_display(rel, &project_prefix_slash));
                } else {
                    eprintln!("  /{rel}");
                }
            }
        }
    }

    // Warn about paths that were written but are not covered by declared outputs.
    // Uncaptured outputs won't be restored on cache hits.
    // Separate project-local writes from workspace writes for clarity.
    if args.report_undeclared {
        let project_in_scope = |rel: &str| -> bool {
            if project_prefix.is_empty() {
                true
            } else {
                rel == project_prefix || rel.starts_with(&format!("{project_prefix}/"))
            }
        };

        let mut project_undeclared: Vec<&str> = Vec::new();
        let mut workspace_undeclared: Vec<&str> = Vec::new();

        for rel in path_writes.iter().map(String::as_str) {
            if declared_outputs.contains(rel) {
                continue;
            }
            if is_report_excluded(rel) {
                continue;
            }
            // Skip paths that no longer exist — these were temporary writes cleaned up
            // during execution and are not meaningful outputs.
            if !workspace_root.join(rel).exists() {
                continue;
            }
            if project_in_scope(rel) {
                project_undeclared.push(rel);
            } else {
                workspace_undeclared.push(rel);
            }
        }

        if !project_undeclared.is_empty() {
            project_undeclared.sort_unstable();
            eprintln!(
                "ccache: warning: {} path(s) written within project but not declared as moon outputs:",
                project_undeclared.len()
            );
            for rel in project_undeclared {
                eprintln!("  {}", project_display(rel, &project_prefix_slash));
            }
        }

        if !workspace_undeclared.is_empty() {
            workspace_undeclared.sort_unstable();
            eprintln!(
                "ccache: warning: {} path(s) written outside project but not declared as moon outputs:",
                workspace_undeclared.len()
            );
            for rel in workspace_undeclared {
                eprintln!("  /{rel}");
            }
        }
    }

    let (archive_key, archive_size) = {
        let workspace_root = workspace_root.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            cache.store_outputs(
                &workspace_root,
                &output_paths,
                stdout.as_file_mut(),
                stderr.as_file_mut(),
            )
        })
        .await
        .context("store_outputs panicked")??
    };

    let execution = Execution::new(inputs, &archive_key, archive_size);
    tokio::task::spawn_blocking(move || cache.store_execution(&execution_key, execution))
        .await
        .context("record_execution panicked")??;

    Ok(())
}

/// Strip the project prefix from a workspace-relative path for display in `--report-undeclared`
/// warnings, returning a project-relative form that matches Moon's `inputs`/`outputs` syntax.
///
/// `project_prefix_slash` must be `"{project_prefix}/"` (or empty when the project is at
/// the workspace root), pre-computed by the caller to avoid repeated allocation.
fn project_display<'a>(ws_rel: &'a str, project_prefix_slash: &str) -> &'a str {
    if project_prefix_slash.is_empty() {
        ws_rel
    } else {
        ws_rel
            .strip_prefix(project_prefix_slash)
            .unwrap_or(ws_rel)
    }
}

/// Build a glob filter for paths that should be suppressed from `--report-undeclared` warnings.
///
/// These are tool-managed directories that Moon tracks via its own toolchain integration
/// (package managers, language runtimes, build caches). They are not meaningful additions
/// to declare explicitly in `inputs` or `outputs`, and including them in warnings produces
/// noise that obscures genuinely missing declarations.
///
/// Note: these suppressions apply only to the warning output. They do not affect the
/// fingerprint — reads from these paths still contribute to execution fingerprinting unless explicitly
/// excluded via `--exclude`. These could be exclude from the execution fingerprinting but requires
/// a better execution key strategy - such as MOON_TASK_FINGERPRINT (or something similar)
fn build_report_exclusions() -> Result<globset::GlobSet> {
    // Tool-managed directories for every toolchain Moon natively supports.
    // Moon already hashes the relevant lockfiles/manifests for each ecosystem,
    // so reads from these directories do not need to be declared as task inputs.
    const PATTERNS: &[&str] = &[
        // Node.js ecosystem: npm, pnpm, yarn (classic + Berry/PnP), Bun, Deno compat mode
        "**/node_modules/**",
        "**/.yarn/**", // Yarn Berry artefacts and offline cache
        // Python ecosystem: pip, uv (venv name is configurable; .venv is the Moon default)
        "**/.venv/**",
        "**/__pycache__/**",
        // Go, PHP (Composer), Ruby (Bundler): installed dependency directories
        // Moon hashes the corresponding lockfile/manifest for each.
        "**/vendor/**",
    ];
    let mut builder = globset::GlobSetBuilder::new();
    for &pat in PATTERNS {
        builder.add(
            globset::Glob::new(pat)
                .with_context(|| format!("invalid built-in report exclusion pattern: {pat}"))?,
        );
    }
    builder
        .build()
        .context("failed to compile report exclusion patterns")
}

/// Coerce a Moon error into an [`anyhow::Error`].
///
/// Moon's error type (`miette::Report`) does not implement `std::error::Error`, so it
/// cannot be converted with `?` directly. We format it with the `{:#}` alternate Display
/// (which includes the full cause chain) and wrap it in a plain `anyhow` error.
pub(crate) fn moon_err(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e:#}")
}

/// Resolve a path environment variable to a canonicalized [`PathBuf`].
///
/// Errors if the variable is unset or if the path does not exist on disk.
fn resolve_env_path(var: &str) -> Result<PathBuf> {
    let p = PathBuf::from(
        env::var_os(var)
            .with_context(|| format!("{var} is not set — ccache must be run via Moon"))?,
    );
    p.canonicalize()
        .with_context(|| format!("{var} does not exist: {}", p.display()))
}
