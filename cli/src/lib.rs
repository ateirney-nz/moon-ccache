//! Moon build command cache.
//!
//! See the project README for full documentation.

mod cache;
mod fingerprint;
mod manifest;
mod path_filter;
mod paths;
mod task_info;
mod tracer;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::path::PathBuf;

use cache::{Cache, Execution, declared_cache_key};
use manifest::build_trace_manifest;
use path_filter::PathFilter;
use paths::to_relative_path;
use task_info::load_task_info;
use tracer::trace_and_run;

#[derive(Parser, Debug)]
#[command(author, version)]
struct Args {
    /// Print cache hit/miss and execution validation details.
    #[arg(long)]
    verbose: bool,

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

    let cache_dir = resolve_env_path("MOON_CACHE_DIR")?.join("ccache");
    let project_root = resolve_env_path("MOON_PROJECT_ROOT")?;
    let snapshot_path = resolve_env_path("MOON_PROJECT_SNAPSHOT")?;
    let target =
        env::var("MOON_TARGET").context("MOON_TARGET is not set — ccache must be run via Moon")?;
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

    let task_info = load_task_info(&target, &snapshot_path, &workspace_root)?;

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
    if args.verbose {
        if input_filter.is_none() {
            eprintln!("ccache: input tracking: all fspy-observed workspace reads");
        } else {
            eprintln!(
                "ccache: input tracking: {} exclude(s) from CLI",
                args.exclude_inputs.len(),
            );
        }
    }

    let cache = Cache::new(&cache_dir, args.verbose)?;

    let declared_key = declared_cache_key(
        &target,
        &args.command,
        &task_info,
        &workspace_root,
        &args.exclude_inputs,
    )?;
    if args.verbose {
        eprintln!(
            "ccache: declared key: {declared_key} (target={target}, command={:?})",
            args.command
        );
    }

    let lookup_start = std::time::Instant::now();
    let hit = {
        let declared_key = declared_key.clone();
        let workspace_root = workspace_root.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            cache.find_matching_execution(&declared_key, &workspace_root)
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
        tokio::task::spawn_blocking(move || cache.restore_from_cas(&execution, &workspace_root))
            .await
            .context("restore_from_cas panicked")??;
        return Ok(());
    }

    if args.verbose {
        eprintln!("cache miss {target} ({:.1?})", lookup_start.elapsed());
    }

    let tracer::TraceResult {
        path_accesses,
        stdout,
        stderr,
    } = trace_and_run(&args.command).await?;

    let manifest = {
        let workspace_root = workspace_root.clone();
        let filter = input_filter.clone();
        tokio::task::spawn_blocking(move || {
            build_trace_manifest(&path_accesses, &workspace_root, filter.as_ref())
        })
        .await
        .context("build_trace_manifest panicked")??
    };
    if args.verbose {
        eprintln!("ccache: traced inputs: {}", manifest.inputs.len());
    }

    let output_paths = task_info.output_files()?;
    if args.verbose {
        eprintln!("ccache: output files: {}", output_paths.len());
    }

    let (archive_key, archive_size) = {
        let workspace_root = workspace_root.clone();
        let cache = cache.clone();
        tokio::task::spawn_blocking(move || {
            cache.create_tarball(&workspace_root, &output_paths, &stdout, &stderr)
        })
        .await
        .context("create_tarball panicked")??
    };

    let execution = Execution::new(&target, &args.command, manifest, &archive_key, archive_size);
    tokio::task::spawn_blocking(move || cache.record_execution(&declared_key, execution))
        .await
        .context("record_execution panicked")??;

    Ok(())
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
