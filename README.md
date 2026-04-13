# ccache — Moon build command cache

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A generic command output cache for [Moon](https://moonrepo.dev) that complements Moon's built-in cache by keying on what a command **actually accesses at runtime** rather than its full transitive input graph. It uses a two-stage lookup: an **execution key** derived from declared task metadata narrows the search to relevant prior runs, then OS-level process tracing via [fspy](https://github.com/voidzero-dev/vite-task/tree/main/crates/fspy) fingerprints the files read and directory listings observed at runtime to find an exact match for cache restoration. This is a proof of concept for a supplemental caching strategy.

## Why This Exists

Moon's built-in cache ([hashing docs](https://moonrepo.dev/docs/concepts/cache#hashing)) keys each task on its declared `inputs`, environment variables, and — critically — its `deps`. When a task lists another task as a dependency, Moon folds that dependency's **inputs** into the hash. This means that if any source file consumed by a dep task changes, the downstream task's cache key changes too, even when the dep task's **output** is byte-for-byte identical to before.

In practice this causes unnecessary churn: a dependency task re-runs, produces the same compiled artifact, but every task that depends on it is forced to re-run anyway because Moon's hash was invalidated by the source change, not the output change.

**ccache** takes a different approach: it keys on the files the command **actually reads at runtime**, captured via OS-level process tracing. If the dep task's output didn't change, the files your command read didn't change, and ccache returns a hit — regardless of what happened upstream at the source level.

This makes ccache a useful complement to Moon's built-in cache for tasks where:

- The output is determined by what the command reads at runtime, not by the full transitive input graph
- Dependency outputs are stable even when dependency inputs churn (e.g. a codegen step whose output is unaffected by whitespace or comment changes in the source)

The mechanism that makes this work is a two-stage lookup: declared task inputs define an **execution key** that narrows the search to relevant prior runs, then runtime process tracing **fingerprints** those candidates — file contents read and directory listings observed — to find an exact match. Directory listings matter because a command may alter its behaviour based on which files exist, not just their contents. See [How It Works](#how-it-works) for details.

## Quick Start

### Prerequisites

- **Rust 1.75+** (uses nightly toolchain as specified in `rust-toolchain.toml`)
- **Moon** environment (ccache provides its own environment variables)

### Building

```bash
# Build release binary for current platform
make build

# Build for specific target
make build TARGET=x86_64-unknown-linux-gnu
```

### Installation

```bash
# Install symlink to target binary
make install

# Or install directly to PATH (requires write access)
cargo install --path ./cli
```

## Usage

Prefix any Moon task command with `ccache --`:

```yaml
# moon.yml or .moon/tasks.yml
tasks:
  build:
    command: ccache -- tsc --build
    inputs:
      - "src/**/*.ts"
      - tsconfig.json
    outputs:
      - dist

  codegen:
    command: >
      ccache
      --exclude '**/node_modules/**'
      --
      protoc --js_out=dist src/api.proto
    inputs:
      - "src/**/*.proto"
    outputs:
      - dist

  audit:
    # Use --report-undeclared to surface gaps in inputs/outputs configuration
    command: ccache --report-undeclared -- npm run build
    inputs:
      - "src/**/*.ts"
    outputs:
      - dist
```

The `--` separator is required to distinguish ccache flags from the wrapped command.

### Flags

- `--verbose` — Print cache hit/miss and execution validation details
- `--report-undeclared` — After each cache miss, warn about paths read/written that are not declared in Moon `inputs`/`outputs`. Tighter declared inputs produce a smaller execution key namespace and fewer candidates to fingerprint-check; they also improve Moon's own execution targeting. See [Undeclared Input/Output Warnings](#undeclared-input-warnings).
- `--exclude <PATTERN>` — Exclude fspy-observed reads matching glob patterns from fingerprint (repeatable)

Patterns with leading `/` are workspace-root relative; others are relative to the package directory (`$MOON_PROJECT_ROOT`).

### Environment Variables

- `MOON_CCACHE_VERBOSE=true` — equivalent to `--verbose`; useful when the task command line cannot be modified

## How It Works

On each run, ccache performs a two-stage lookup. First, it computes an **execution key** from static task metadata — this key namespaces all prior executions for a given task variant, bounding the candidate set to relevant runs. Second, it searches that candidate set by validating each execution's **trace manifest** — a fingerprint of every filesystem read operation observed at runtime: file contents read, and directories listed. Directory listings are tracked because a command may change its behaviour based on which files exist in a directory, not just their contents. If a prior execution's trace manifest still matches the current workspace, ccache restores its output archive and replays stdout/stderr without running the command.

The precision of the execution key matters for lookup efficiency: a tighter key (from more precisely declared inputs) produces a smaller candidate set, meaning fewer trace manifests need to be evaluated to find a match.

### Execution Key

The execution key is an XXH3 hex digest of task metadata — nothing is read from disk beyond what Moon already resolved:

- The Moon target name
- The exact command line
- XXH3 content hash of all declared input files (from the Moon snapshot)
- Task-declared `env` values (hardcoded env key-value pairs from `moon.yml`)
- Runtime values of `input_env` variables (resolved from the process environment at run time)
- Any `--exclude` patterns

This mirrors the env-variable contribution in Moon's own task hasher. It does **not** include dependency task hashes — that exclusion is the whole point (see [Why This Exists](#why-this-exists)).

The execution key's role is to namespace prior executions for a given task variant, bounding the candidate set for trace manifest evaluation. It does not determine whether a cache hit occurs — that decision is made by fingerprint matching within the candidate set. A more precise key (from well-declared inputs) produces a smaller candidate set and therefore more efficient lookups; an imprecise or overly broad key produces more candidates, each of which must be fingerprint-checked before a hit or miss is confirmed.

### Cache Hit

1. Compute the declared key and list executions under `$MOON_CACHE_DIR/ccache/manifests/<execution-key>/`
2. Sort executions newest-first by filename (timestamp prefix makes lexicographic sort correct; UUID suffix prevents collisions between concurrent writers)
3. Skip executions with an incompatible `schema_version`; consider at most the newest 10
4. For each execution (newest first), validate its trace manifest against the current workspace:
   - **Metadata check** (stat only): every recorded input must still exist with the same size — no file reads
   - **Digest check** (read + fingerprint): full path fingerprints must match; results are memoised across executions so each path is read at most once; parallelised via rayon
5. On the first match: verify the output archive's compressed size, extract output files under workspace root, and replay stdout/stderr to the terminal

### Cache Miss

1. Run command under `fspy` to intercept file reads/writes; SIGINT/SIGTERM are forwarded to the child process
2. Collect path accesses in a single pass: separate reads from writes, OR the `READ_DIR` flag across multiple accesses on the same path, and discard paths outside the workspace root and under `.git`
3. Apply `--exclude` filters and fingerprint surviving read-but-not-written paths in parallel via rayon
4. If `--report-undeclared` is set: warn about project paths that were exclusively read but are not declared as Moon `inputs` (see [Undeclared Input Warnings](#undeclared-input-warnings))
5. If `--report-undeclared` is set: warn about paths that were written but are not covered by declared `outputs` (see [Undeclared Output Warnings](#undeclared-output-warnings))
6. Record the new execution as zstd-compressed JSON: `manifests/<execution-key>/<YYYYMMDD-HHMMSS.mmm>-<uuid8>.json.zst`
7. Pack declared output files + captured stdout/stderr into a zstd tarball; SHA256 of the compressed bytes becomes the archive key (`sha256:<hex>.tar.zst`)

### Undeclared Input Warnings

When `--report-undeclared` is set, after each cache miss ccache compares the files it observed being read (within the current project directory) against the task's declared `inputs`. Any path that was exclusively read — not written — and is absent from the declared inputs is printed as a warning to stderr:

```
ccache: warning: 2 path(s) read within project but not declared as moon inputs:
  packages/my-app/src/config.ts
  packages/my-app/tsconfig.json
```

These warnings don't affect caching correctness — ccache already incorporates all observed reads into its fingerprint regardless of whether they are declared. But they have two practical benefits: adding missing paths to `inputs` tightens the execution key namespace (fewer candidate executions to fingerprint-check per lookup), and improves Moon's own execution targeting so it correctly skips or re-runs the task based on those files changing.

Tool-managed directories for all toolchains Moon natively supports are automatically suppressed from these warnings — `node_modules` and `.yarn` (Node.js/npm/pnpm/yarn/Bun/Deno), `.venv` and `__pycache__` (Python/pip/uv), and `vendor` (Go, PHP/Composer, Ruby/Bundler). Warnings that do appear represent paths that are neither declared nor covered by Moon's toolchain integration.

This is intentionally limited to paths **within the project**. Reads from outside the project boundary (other packages, workspace-level config, dependency outputs) are handled by ccache's observed-read fingerprint rather than Moon's declared input graph — that separation is the core of ccache's approach (see [Why This Exists](#why-this-exists)).

That said, for workspace-level paths that are **known** real inputs — shared config files, root-level toolchain configs — you can still declare them explicitly in `inputs` using Moon's workspace-relative syntax (a leading `/`):

```yaml
tasks:
  build:
    command: ccache -- tsc --build
    inputs:
      - "src/**/*.ts"
      - tsconfig.json
      - /tsconfig.base.json     # shared workspace-root config
      - /.eslintrc.yml          # workspace-level tooling config
    outputs:
      - dist
```

Declaring these paths contributes them to the execution key (see [Execution Key](#execution-key)), which tightens the candidate namespace: two runs that differ only in a shared config will get distinct keys and therefore non-overlapping candidate sets, rather than landing in the same bucket and relying on trace manifest matching to distinguish them. Cross-package dependencies should still be expressed via `deps` rather than listing another package's output paths as inputs.

### Undeclared Output Warnings

When `--report-undeclared` is set, after each cache miss ccache checks whether any files written during execution are missing from the task's declared `outputs`. Warnings are split into two categories:

**Project-local outputs** — files written within the project directory that aren't declared:

```
ccache: warning: 2 path(s) written within project but not declared as moon outputs:
  packages/my-app/.tsbuildinfo
  packages/my-app/dist/debug.log
```

These are typically legitimate outputs that should be added to the task's `outputs` configuration to ensure they're captured and restored on cache hits.

**Workspace outputs** — files written outside the project but within the workspace:

```
ccache: warning: 1 path(s) written outside project but not declared as moon outputs:
  packages/other-app/generated.json
```

Cross-project writes are unusual and may indicate unintended side effects or shared build artifacts that should be explicitly declared. Investigate whether these are expected before adding them to `outputs`.

Undeclared outputs are not captured in ccache's output archive, which means they won't be restored on cache hits — potentially leaving the workspace in an inconsistent state.

The same tool-managed directory exclusions that apply to input warnings also apply here. Additionally, paths that were written and then deleted before the command exited are not reported — temporary files cleaned up during execution are not meaningful outputs.

### Path Fingerprints

Each tracked path is stored as one of three variants:

- `NotFound` — path did not exist at fingerprint time
- `File { hash, size }` — XXH3-64 digest + byte length; size enables a fast stat pre-check before reading
- `Directory(None)` — directory was opened but not listed (fspy `READ` only); only presence is tracked
- `Directory(Some(entries))` — directory contents were enumerated (fspy `READ_DIR`); entry names and kinds are captured so additions or removals are detected on the next lookup

## Development

### Code Style

This project uses:
- **rustfmt** for formatting (see `.rustfmt.toml`)
- **clippy** for linting
- **cargo test** for testing

### Common Tasks

```bash
# Format code
make fmt

# Check formatting
make fmt-check

# Run clippy linter
make lint

# Run all checks
make check

# Run tests
make test

# Clean build artifacts
make clean
```

See `Makefile` for all available targets.

## Implementation Notes

This is a **proof of concept**. There may be edge cases not yet handled:

- Non-deterministic reads based on timestamps, environment state, or random seeds could produce false cache hits
- Tools with undeclared side effects may not cache correctly
- Process tracing requires OS-level support (Linux, macOS)

Evaluate carefully before use in environments requiring strict cache correctness.

### Tighter Moon Integration

The execution key is currently computed by ccache itself using the snapshot. A cleaner approach would be for Moon to expose the task's own fingerprint — everything that goes into its hash **except** dependency hashes — as an environment variable (e.g. `MOON_TASK_FINGERPRINT`) available to the task at run time.

ccache could then use that value directly as the execution key, eliminating the custom input-hashing logic and guaranteeing alignment with however Moon evolves its own hashing (language-specific factors, config files, etc.). The dep-excluding behaviour that makes ccache useful would fall out naturally, since Moon would be providing the hash of just this task's own immediate inputs.

This would also unlock a second improvement: the same tool-managed directory patterns currently suppressed from `--report-undeclared` warnings (`node_modules`, `.venv`, `vendor`, etc.) could be applied to the **execution fingerprint** itself as default `--exclude` patterns. Today that would be unsafe — ccache computes its own execution key and has no visibility into Moon's toolchain state, so excluding `node_modules` from the fingerprint risks a false hit if a package changes. With `MOON_TASK_FINGERPRINT`, Moon's key already incorporates lockfile/manifest hashing for each ecosystem, making those exclusions safe and reducing the number of paths ccache needs to fingerprint on every miss.

## License

MIT License — see [LICENSE](LICENSE) for details.

## References

- [Moon Documentation](https://moonrepo.dev)
- [fspy — File system process spy](https://github.com/voidzero-dev/vite-task/tree/main/crates/fspy)
- [Rust Edition Guide](https://doc.rust-lang.org/edition-guide/)
