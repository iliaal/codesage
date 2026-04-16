# CodeSage

Code intelligence engine with structural graph and semantic search. Rust workspace, tree-sitter parsing, ONNX embedding inference, cross-encoder reranking, SQLite storage, MCP interface.

## Build

```bash
cargo build                                    # all crates
cargo build --release -p codesage --features cuda  # release binary with GPU
cargo test --workspace                         # all tests
cargo clippy --workspace                       # lint
```

> Always build with `--features cuda` when targeting GPU. Without it, CUDA silently falls back to CPU. The binary will error out if GPU is requested in config but the cuda feature is missing.

## Crate map

| Crate | Role | Depends on |
|-------|------|------------|
| `protocol` | Shared types (Symbol, Reference, SearchResult, etc.) | nothing |
| `parser` | File discovery, language detection, tree-sitter symbol/reference extraction | protocol |
| `storage` | SQLite schema, CRUD, sqlite-vec KNN | protocol |
| `embed` | ONNX embedding inference (Embedder), cross-encoder reranking (Reranker), chunking | ort, tokenizers, hf-hub |
| `graph` | Indexing orchestration, search pipeline, query API | parser, storage, embed, protocol |
| `cli` | `codesage` binary: CLI subcommands + MCP server | everything |

## Search pipeline

Query flows through these stages in order:

1. **Embed query** -- MiniLM-L6-v2 (384d) via ONNX Runtime
2. **KNN retrieval** -- sqlite-vec, overfetch 5x when reranker is active
3. **Symbol boost** -- +0.1 per query token that matches a known symbol in the chunk
4. **Cross-encoder rerank** -- ms-marco-MiniLM-L6-v2, blended 50/50 with semantic score
5. **Symbol annotation** -- attach overlapping symbol names to each result
6. **Truncate** to requested limit

The reranker is optional (configured per-project in config.toml). Without it, steps 2-3-5 still run.

## Config

Per-project config lives at `.codesage/config.toml`:

```toml
[project]
name = "my-project"

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"
device = "gpu"
reranker = "cross-encoder/ms-marco-MiniLM-L6-v2"

[index]
exclude_patterns = [
  "**/tests/**", "**/test/**", "**/__tests__/**",
  "**/*Test.php", "**/*.test.ts", "**/*.spec.ts",
  "**/test_*.py", "**/*_test.py", "**/*.phpt",
  "**/vendor/**", "**/node_modules/**",
]
```

## CUDA setup

ONNX Runtime loads dynamically. CUDA libraries come from pip-installed `nvidia-*-cu12` packages. At first use, the binary discovers them in this order:

1. `CODESAGE_NVIDIA_LIBS` env var, if set (an explicit nvidia root directory).
2. Python `site.getsitepackages()` + `site.getusersitepackages()`, joined with `/nvidia`. Works with both system-wide pip installs and `--user` installs.
3. Standard system paths: `/usr/lib/x86_64-linux-gnu/nvidia`, `/usr/local/lib/nvidia`, `/opt/nvidia`.

`ORT_DYLIB_PATH` can override the ONNX Runtime library location. Left unset, the binary probes the same site-packages locations for `libonnxruntime.so*`.

`codesage doctor` reports how many nvidia lib dirs were discovered and warns if none.

If CUDA is requested (`device = "gpu"`) but fails to register, the process errors out instead of falling back to CPU. This is intentional -- silent CPU fallback produces different embeddings and slower performance.

Required pip packages: `onnxruntime-gpu`, `nvidia-cudnn-cu12`, `nvidia-cublas-cu12`, `nvidia-cuda-runtime-cu12`, `nvidia-cufft-cu12`, `nvidia-curand-cu12`, `nvidia-cuda-nvrtc-cu12`.

## Conventions

- Rust 2024 edition
- `anyhow` in binaries, types in protocol crate
- Tree-sitter queries in `.scm` files under `crates/parser/src/queries/`, embedded via `include_str!`
- JSON output on all query commands (`--json`)
- Model-specific vec0 tables (`chunks_{model}_{dim}`) allow switching models without re-indexing structural data

## Versioning and changelog

This repo follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html). Workspace version lives in `[workspace.package] version` in the root `Cargo.toml`; all six crates inherit it via `version.workspace = true`.

**Every code change that is user-visible must update `CHANGELOG.md` in the same commit.** User-visible means: new CLI flags or subcommands, new or changed MCP tools, behavior changes, breaking changes, new dependencies, schema migrations, hook template changes, config surface changes, and security fixes. Pure internal refactors, test-only changes, doc-only changes, and dev-tooling tweaks don't need an entry.

- Put entries under `## [Unreleased]` at the top of the file, in one of the standard sections: `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`.
- One bullet per change, written for the reader of the next release notes — describe the user-observable effect, not the implementation.

### Cutting a release

1. Move everything under `## [Unreleased]` into a new `## [X.Y.Z] - YYYY-MM-DD` section. Leave `## [Unreleased]` empty above it.
2. Append a link reference at the bottom of `CHANGELOG.md`: `[X.Y.Z]: https://github.com/iliaal/codesage/releases/tag/vX.Y.Z` and update the `[Unreleased]` compare URL to `...vX.Y.Z...HEAD`.
3. Bump `[workspace.package] version` in the root `Cargo.toml`. All six crates inherit it.
4. Commit: `git commit -am "release: vX.Y.Z"`.
5. Tag: `git tag -a vX.Y.Z -m "codesage X.Y.Z"`.
6. Push: `git push origin master && git push origin vX.Y.Z`.

The `Release` workflow (`.github/workflows/release.yml`) fires on the tag push, extracts the matching `[X.Y.Z]` section from `CHANGELOG.md`, and creates a GitHub Release with those notes plus the auto-attached source tarball. If the section is empty or missing, the workflow fails.

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps are backwards-compatible within a minor line.

## Languages

PHP, Python, C, Rust, JavaScript, TypeScript, Go.

## MCP tools

- `search` -- semantic search with embedding + reranking
- `find_symbol` -- symbol definitions by name
- `find_references` -- references to a symbol
- `list_dependencies` -- file-level imports/imported-by
- `impact_analysis` -- files affected by changing a symbol or file, with distance and reasons
- `export_context` -- curated code bundle for a query or symbol, optionally with callers/callees
- `find_coupling` -- files that historically change with a given file (V2b)
- `assess_risk` -- risk score for a single file (V2b slice 1)
- `assess_risk_diff` -- aggregate risk for a patch / set of files (V2b slice 2)
- `recommend_tests` -- tests an agent should run after editing a set of files (V2b slice 2)

## CLI commands

`init`, `index`, `search`, `find-symbol`, `find-references`, `dependencies`, `impact`, `export`, `status`, `mcp`, `install-hooks`, `cleanup`, `git-index`, `coupling`, `risk`, `risk-diff`, `tests-for`, `doctor`.

`cleanup` drops orphaned vec tables from previous model switches, keeping only the active model. Use after benchmarking multiple models. Runs VACUUM automatically.

## Benchmarks

Benchmark harness under `bench/`:

- `bench/codesage-bench-runner` — Python runner that executes a YAML corpus of ground-truth cases against `codesage search` and reports miss rate, median first-hit, recall@5, recall@10.
- `bench/extract-eval-cases.py` — mines eval cases from Claude Code session transcripts and git commit history.
- `bench/cleanup-orphan-models.sh` — drops orphaned vec tables from prior model switches.

Corpus YAMLs are not bundled; bring your own. `CODESAGE_BENCH_CORPUS_DIR` (consumed by `/codesage-bench` and `/codesage-eval` plugin commands) points the plugin at the directory holding them.

## Plugin

`plugins/codesage-tools/` ships as a Claude Code plugin: one global `codesage` MCP serves every onboarded project, routed by an absolute `project` argument. Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. Marketplace manifest at repo root.

## Git history intelligence (V2b slice 1)

`codesage git-index` runs `git log --numstat` and populates `git_files` (per-file churn score with τ=180d decay, fix count, total commits, last commit), `git_co_changes` (file pair weights, min count 3), and `git_index_state` (last indexed SHA).

Three modes, selected via flags on `codesage git-index`:

- `--full`: fresh rescan. Drops existing rows and walks the entire history. Use after big rebases that rewrite a lot of history, or to rebaseline weekly.
- `--incremental`: scans only `<last_sha>..HEAD` and additively updates counters. Scales pre-existing weights by `exp(-Δt/τ)` so exponential decay stays mathematically exact across runs. Sub-threshold co-change pairs that straddle the incremental boundary are approximated (full rescan resolves them).
- default (no flag, `Auto`): incremental if valid prior state exists and its SHA is an ancestor of HEAD, else full.

`codesage install-hooks` now registers `post-commit`, `post-merge`, `post-checkout`, and `post-rewrite`, each running `codesage git-index --incremental` in the background. Rebased or force-updated history triggers a full rescan automatically (incremental detects when the stored SHA is no longer an ancestor of HEAD and falls back to full).

Two MCP tools consume the tables:

- `find_coupling(project, file_path, limit)` -- top-N files that historically change together with the input, weight-sorted. CLI: `codesage coupling <file>`.
- `assess_risk(project, file_path)` -- composite risk score (0..1) from churn percentile + fix ratio + depth-2 reverse-dep pressure + coupling pressure + test gap. Returns decomposition and human-readable notes for PR descriptions. CLI: `codesage risk <file>`.

The indexer filters the same `DEFAULT_EXCLUDE_PATTERNS` as the structural indexer, plus NEWS/UPGRADING/CHANGELOG variants (they touch every commit so they pollute coupling).

## Roadmap

V1: semantic retrieval + structural graph + MCP interface, change impact analysis, context export, plugin-based deployment.

V2b slice 1 (shipped 0.2.0): git history intelligence — `find_coupling` + `assess_risk` MCP tools, `codesage git-index` CLI with incremental hooks.

V2b slice 2 (next): `bus_factor`, `change_pattern`, `find_hotspots` MCP tools. Conditional on slice 1 validating on large real codebases.

V2c (deferred): docs/decision layer (process traces, architecture summaries). Revisited after V2b slice 2 lands.
