# ccache — Moon build command cache

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

A generic command output cache for [Moon](https://moonrepo.dev), using [fspy](https://github.com/voidzero-dev/vite-task/tree/main/crates/fspy) process tracing to automatically discover undeclared workspace inputs at runtime and incorporate them into an execution fingerprint for cache restoration. This is a proof of concept for a supplemental caching strategy that complements Moon's existing cache.

## Overview

Unlike Moon's built-in cache, which relies entirely on declared `inputs`, **ccache** wraps any command and observes which files it actually reads using OS-level process tracing (writes are tracked only to exclude them from the input fingerprint). This makes it useful for scenarios where:

- The declared task inputs, combined with reads from the rest of the workspace, **largely determine the output**
- For a given set of inputs, the command reads a stable and predictable set of files and produces the same output if they are unchanged

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
```

The `--` separator is required to distinguish ccache flags from the wrapped command.

### Flags

- `--verbose` — Print cache hit/miss and execution validation details
- `--exclude <PATTERN>` — Exclude fspy-observed reads matching glob patterns from fingerprint (repeatable)

Patterns with leading `/` are workspace-root relative; others are relative to the package directory (`$MOON_PROJECT_ROOT`).

### Environment Variables

- `MOON_CCACHE_VERBOSE=true` — equivalent to `--verbose`; useful when the task command line cannot be modified

## How It Works

On each run, ccache computes a **declared key** from static task metadata, then searches prior **executions** stored under that key. Each execution holds a **trace manifest** — the fingerprinted set of files the command actually read at runtime. If a prior execution's trace manifest still matches the current workspace, ccache restores its output archive and replays stdout/stderr without running the command.

### Declared Key

The declared key is an XXH3 hex digest of static information only — nothing is read from disk beyond what Moon already resolved:

- The Moon target name
- The exact command line
- XXH3 content hash of all declared input files (from the Moon snapshot)
- Any `--exclude` patterns

This key namespaces all recorded executions for a given task variant, bounding the search to relevant prior runs.

### Cache Hit

1. Compute the declared key and list executions under `$MOON_CACHE_DIR/ccache/manifests/<declared-key>/`
2. Sort executions newest-first by filename (timestamp prefix makes lexicographic sort correct; UUID suffix prevents collisions between concurrent writers)
3. Skip executions with an incompatible `schema_version`; consider at most the newest 10
4. For each execution (newest first), validate its trace manifest against the current workspace:
   - **Metadata check** (stat only): every recorded input must still exist with the same size — no file reads
   - **Digest check** (read + fingerprint): full path fingerprints must match; results are memoised across executions so each path is read at most once; parallelised via rayon
5. On the first match: verify the output archive's compressed size, extract output files under workspace root, and replay stdout/stderr to the terminal

### Cache Miss

1. Run command under `fspy` to intercept file reads/writes; SIGINT/SIGTERM are forwarded to the child process
2. Build the trace manifest in three passes:
   - **Pass 1**: collect all written paths to exclude from inputs
   - **Pass 2**: collect paths that were read-but-not-written under workspace root; apply `--exclude` filters; OR the `READ_DIR` flag across multiple accesses on the same path
   - **Pass 3**: fingerprint surviving input paths in parallel via rayon
3. Record the new execution as JSON: `manifests/<declared-key>/<YYYYMMDD-HHMMSS.mmm>-<uuid8>.json`
4. Pack declared output files + captured stdout/stderr (up to 64 MiB each) into a gzip tarball; SHA256 of the compressed bytes becomes the archive key (`sha256:<hex>.tar.gz`)

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

## License

MIT License — see [LICENSE](LICENSE) for details.

## References

- [Moon Documentation](https://moonrepo.dev)
- [fspy — File system process spy](https://github.com/voidzero-dev/vite-task/tree/main/crates/fspy)
- [Rust Edition Guide](https://doc.rust-lang.org/edition-guide/)
