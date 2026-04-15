# Changelog

All notable changes to CodeSage are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps are backwards-compatible
within the same minor line. Once we reach 1.0.0, the standard SemVer contract applies.

## [Unreleased]

## [0.2.0] - 2026-04-15

Initial public release.

### Added

- **Git history intelligence (V2b slice 1)**: `git_files` + `git_co_changes` tables, populated
  by `codesage git-index`. Per-file churn score with 180-day exponential decay, fix-ratio
  tracking, and pairwise co-change weights (min count 3). Two MCP tools consume the tables:
  - `find_coupling(project, file_path, limit)` — top-N files that historically change together,
    weight-sorted. CLI: `codesage coupling <file>`.
  - `assess_risk(project, file_path)` — composite risk score from churn percentile + fix ratio +
    depth-2 dependent pressure + coupling pressure + test gap. Emits human-readable notes for
    PR descriptions. CLI: `codesage risk <file>`.
- **Incremental git-index** with auto-refresh via git hooks. `codesage git-index` gains
  `--full` / `--incremental` flags (default: auto — incremental if prior state is valid, else
  full). New `git_index_state` table stores the last indexed SHA. Head-unchanged short-circuits
  in ~5 ms vs ~14 s for a full rescan on mid-size repos. Rebased/force-pushed history auto-falls
  back to full when the stored SHA stops being an ancestor of HEAD.
- **`codesage doctor`** subcommand with 8 health checks: binary, config, DB, disk space, hooks,
  CUDA, models, MCP registration. Supports `--json`.
- **`codesage export --format=ingest`** flat-text envelope (gitingest-style) for dropping a
  curated code bundle into another LLM context.
- **`codesage-tools` Claude Code plugin** with slash commands: `/codesage-onboard`,
  `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. One global MCP
  server routes to every onboarded project via an absolute `project` argument.
- **Cross-encoder reranking** (ms-marco-MiniLM-L6-v2) blended 50/50 with semantic score,
  overfetch 5x when the reranker is active. Configurable per-project in `config.toml`.
- **Change impact analysis**: `codesage impact <symbol|file>` + `impact_analysis` MCP tool.
  Depth-configurable reverse-dependency walk with distance and reason annotations.
- **Husky-aware `install-hooks`**: detects `core.hooksPath` + Husky marker files, writes to
  `.husky/<name>`, and adds installed paths to `.git/info/exclude` so tooling hooks don't show
  up as working-tree changes. Now installs four hooks: `post-commit`, `post-merge`,
  `post-checkout`, `post-rewrite`.
- **Hook templates are worktree-aware**: hook body uses `git rev-parse --show-toplevel` at
  runtime, so one hook serves every worktree under a shared `.git/` common dir.
- **Expanded default ignore patterns** (~85 entries). User `exclude_patterns` is now additive:
  user config extends the defaults rather than replacing them. Changelog files (NEWS,
  UPGRADING, CHANGELOG, HISTORY, RELEASE_NOTES) are excluded by default since they touch
  every commit and pollute co-change.
- **MCP token budget caps** (~8000 tokens) with truncation metadata, so large tool outputs
  degrade gracefully instead of overflowing downstream model contexts.
- **JavaScript and TypeScript support** (tree-sitter queries, `require()` capture, exported
  const extraction).
- **Workspace-level versioning** via `[workspace.package]` version, with each crate using
  `version.workspace = true`. All six crates now bump together.
- **CUDA / ONNX Runtime library discovery** is now portable. The binary probes in order:
  `CODESAGE_NVIDIA_LIBS` env var → Python `site.getsitepackages()` + user site-packages →
  standard system paths (`/usr/lib/x86_64-linux-gnu/nvidia`, `/usr/local/lib/nvidia`,
  `/opt/nvidia`). `ORT_DYLIB_PATH` still overrides the ONNX Runtime location. `codesage
  doctor` reports how many NVIDIA lib dirs were discovered.
- **Bench plugin commands honor `CODESAGE_BENCH_CORPUS_DIR`** (falls back to `./bench-corpora`
  in the working directory). No hardcoded personal paths.

### Changed

- Indexing pipeline refactored: full + incremental variants collapsed into one `IndexStrategy`,
  same transformation for both paths. Structural and semantic indexers share the same
  strategy object.
- Parallelized parsing via Rayon; SQLite writes batched to cut indexing time on large repos.
- Reference resolution now uses a `to_name_tail` index for suffix matching across PHP
  namespaces, Rust paths, and filesystem-style names. Backed by a schema migration that
  populates the column on existing databases.
- Search results annotated with overlapping structural symbols (class, function) for easier
  triage.
- MCP server is multi-project: one global registration, tools receive `project` argument to
  route. Removes the need for per-project MCP entries.

### Fixed

- Husky projects where `core.hooksPath = .husky/_`: install-hooks previously wrote to
  `.git/hooks/` which was never invoked. Now detects and writes to the user Husky dir.
- Worktree onboarding: `codesage-onboard` previously checked `[ -d .git ]` which is false in
  worktrees (where `.git` is a file pointing at the common dir). Now uses `-e`.
- Hook-template sibling worktrees without `.codesage/`: hook guarded with `[ -d "$root/.codesage" ]`
  so a worktree without an index no longer errors on every commit.
- `.gitignore` pollution: codesage's own ignore rules now go to `.git/info/exclude` (per
  worktree when applicable) instead of the tracked `.gitignore`.
- Schema migration ordering: `idx_refs_to_name_tail` is now created unconditionally at the
  end of `migrate_refs_to_name_tail`, not as part of the initial SCHEMA batch. Fixes onboarding
  failure on legacy databases.
- C reference extraction previously dropped all function calls; now captured.
- PHP cross-file reference resolution now uses suffix matching on qualified names.

### Removed

- `thiserror` dependency.
- Dead stubs in `impact_test.rs`; unused reference-expansion code (kept in tree, disabled in
  the search path pending a future evaluation).

[Unreleased]: https://github.com/iliaal/codesage/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/iliaal/codesage/releases/tag/v0.2.0
