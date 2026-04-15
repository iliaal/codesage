# Changelog

Track notable changes to CodeSage here.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and releases follow [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps stay backwards-compatible within the same minor line. Standard SemVer applies at 1.0.0.

## [Unreleased]

## [0.3.2] - 2026-04-15

Performance fix.

### Fixed

- `codesage git-index` was issuing one fsync per upserted row. On large repos like php-src (~25k files + ~10k pairs per worktree) this burned multi-minute disk waits in `jbd2_log_wait_commit` — visible as ~1% CPU per process. Now wraps clear + every upsert in a single SQLite transaction (one fsync per indexer instead of N). Estimated 10-20x speed-up on large repos with the same disk; bigger when multiple indexers share the same disk journal.

[0.3.2]: https://github.com/iliaal/codesage/releases/tag/v0.3.2

## [0.3.1] - 2026-04-15

Bug-fix release for two issues exposed by dogfooding 0.3.0 against php-src.

### Fixed

- `codesage git-index` was dropping test-like files from co-change pair generation entirely, which made `recommend_tests`'s `coupled` section empty for codebases where tests are the primary partner of source changes (e.g., php-src `.c` ↔ `.phpt`). Now only test↔test pairs are skipped (which is the actual noise); source↔test pairs are kept (which is the signal `recommend_tests` needs).
- `recommend_tests` for `.c` / `.h` source files now lists `.phpt` tests in `<dir>/tests/` as primary (php-src convention). Skips when the `tests/` directory has more than 50 files (typical of `ext/standard/tests/`) — that's too noisy for "primary" and the agent should rely on coupling history instead.

### Required action

Re-run `codesage git-index --full` in any repo that was indexed under 0.3.0 to repopulate co-change pairs with the corrected logic.

[0.3.1]: https://github.com/iliaal/codesage/releases/tag/v0.3.1

## [0.3.0] - 2026-04-15

V2b slice 2: tools that change agent behavior per-task instead of just informing it.

### Added

- **`assess_risk_diff(project, file_paths[])`** MCP tool and `codesage risk-diff` CLI command. Aggregate risk for a set of files (the file list of a patch). Returns per-file decomposition plus rollups: `max_score`, `mean_score`, `max_risk_file`, and lists of files in each risk category (hotspot, fix-heavy, test-gap, wide blast radius). Output includes paste-ready `summary_notes` for PR descriptions. Use BEFORE submitting a patch to decide whether to add tests, split the change, or flag concerns.
- **`recommend_tests(project, file_paths[])`** MCP tool and `codesage tests-for` CLI command. Returns the tests an agent should run after editing a set of files. Two layers: `primary` (sibling tests resolved by language convention — `FooTest.php`, `foo.test.ts`, `test_foo.py`, `foo_test.go`, plus Rust integration tests under `crates/<name>/tests/*.rs`) and `coupled` (tests that historically change with the input files via git co-change history). Coupled entries are deduped against primary so each test appears once. Empty result means no test files in the index for these paths.
- Both new CLI commands accept positional file args or read newline-separated paths from stdin, so they compose with `git diff --name-only | codesage risk-diff` and similar pipelines.
- `discover::TEST_LIKE_EXCLUDE_PATTERNS`: subset of the default exclude list covering test and bench files. Exposed so `git_history` can split test files from hard-excluded files.

### Changed

- `codesage git-index` now keeps test and bench files in `git_files` so `recommend_tests` and `assess_risk` test-gap detection can find them. Test files remain dropped from co-change pair generation, so coupling rankings stay focused on production code. Re-run `codesage git-index --full` after upgrading to populate test files.

[Unreleased]: https://github.com/iliaal/codesage/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/iliaal/codesage/releases/tag/v0.3.0

## [0.2.1] - 2026-04-15

Hardening release. New defensive mechanisms around accidentally committing private data plus the public-release CI surface (CI, secret scan, automated release notes from CHANGELOG).

### Added

- `codesage install-hooks` now installs a `pre-commit` hook when the repo contains `scripts/leak-check.sh`. The hook greps staged content against extended-regex patterns from `scripts/leak-patterns.txt` (tracked, shared) and `.git/info/leak-patterns.txt` (local-only, per-developer) and blocks the commit on a match. Bypass with `git commit --no-verify` when a false positive is intentional.
- `scripts/leak-patterns.txt` with default patterns for private-key material and common token formats (AWS, GitHub PATs, Slack, Stripe live keys).
- Pre-commit filename policy. The hook blocks any staged file whose name matches a secret/credential pattern: `.env*` (except `.example`, `.template`, `.sample`), `.secret`, `.secrets/`, `*.pem`, `*.p12`, `*.pfx`, `id_rsa*` / `id_dsa*` / `id_ecdsa*` / `id_ed25519*` (except `.pub` public-key variants), `credentials.json`, `service-account*.json`.
- `scripts/leak-check.sh` supports `--range A..B` and `--all` modes so the same script powers both the pre-commit hook and CI.

### Changed

- `.gitignore` excludes benchmark artifacts under `bench/` (results, corpora, history, scorecards) and common secret filenames (`.env*`, `*.pem`, `id_rsa*`, `credentials.json`, etc.) so local data never enters a commit by default. Template files (`.env.example`, `.env.template`, `.env.sample`) remain committable.

## [0.2.0] - 2026-04-15

Initial public release.

### Added

- Semantic search via MiniLM-L6-v2 embeddings (384d) with optional cross-encoder reranking (ms-marco-MiniLM-L6-v2). Query-token symbol boost runs before rerank; final scores blend 50/50.
- Structural graph: symbol definitions, references (imports, calls, inheritance), and file-level dependencies. Tree-sitter queries cover PHP, Python, C, Rust, JavaScript, and TypeScript.
- Change impact analysis via `codesage impact <symbol|file>` and the `impact_analysis` MCP tool. Walks reverse dependencies to a configurable depth and annotates each hop with distance and reason.
- Context export to JSON, markdown, or flat-text (gitingest-style) via `codesage export` and the `export_context` MCP tool. Optionally includes callers and callees.
- Git history intelligence. `codesage git-index` populates per-file churn (180-day exponential decay), fix ratio, total commits, last commit, and pairwise co-change weights. Two MCP tools consume it:
  - `find_coupling(project, file_path, limit)`: top files that historically change together (CLI: `codesage coupling <file>`).
  - `assess_risk(project, file_path)`: composite score from churn percentile, fix ratio, depth-2 dependent pressure, coupling pressure, and test gap. Emits notes you can paste into a PR description (CLI: `codesage risk <file>`).
- Incremental git history indexing with auto-refresh via git hooks. `codesage git-index` takes `--full` or `--incremental`; the default is auto (incremental when prior state is valid, else full). Rebased or force-pushed history falls back to full when the stored commit is no longer an ancestor of HEAD. Re-running against an unchanged HEAD returns in ~5 ms.
- `codesage doctor` health checks: binary, config, database, disk space, hooks, CUDA, models, MCP registration. Supports `--json`.
- `codesage-tools` Claude Code plugin. Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. One global MCP server, routed per call by an absolute `project` argument.
- Husky-aware `codesage install-hooks`. Detects `core.hooksPath` and Husky markers, writes to `.husky/<name>`, and adds installed paths to `.git/info/exclude`. Installs `post-commit`, `post-merge`, `post-checkout`, and `post-rewrite`.
- Worktree-aware hooks. Each hook body uses `git rev-parse --show-toplevel` at runtime, so a single hook serves every worktree under a shared `.git/` common dir.
- Expanded default ignore patterns (~85 entries) covering tests, vendored dependencies, build outputs, and language caches. User `exclude_patterns` in `config.toml` extend the defaults instead of replacing them. Changelog files (NEWS, UPGRADING, CHANGELOG, HISTORY, RELEASE_NOTES) are excluded because they touch every commit and pollute co-change.
- MCP responses cap at ~8000 tokens with truncation metadata. Large outputs degrade gracefully instead of overflowing downstream contexts.
- Parallel indexing via Rayon with batched SQLite writes.
- Portable CUDA and ONNX Runtime discovery. The binary checks `CODESAGE_NVIDIA_LIBS`, then Python `site.getsitepackages()` and user site-packages, then standard system paths (`/usr/lib/x86_64-linux-gnu/nvidia`, `/usr/local/lib/nvidia`, `/opt/nvidia`). `ORT_DYLIB_PATH` overrides the ONNX Runtime location. `codesage doctor` reports how many NVIDIA lib directories it found.
- Bench plugin commands honor `CODESAGE_BENCH_CORPUS_DIR` (default: `./bench-corpora`). No hardcoded personal paths.
- Workspace-level versioning. `[workspace.package]` owns the version; every crate inherits via `version.workspace = true`, so releases bump in one line.

[0.2.1]: https://github.com/iliaal/codesage/releases/tag/v0.2.1
[0.2.0]: https://github.com/iliaal/codesage/releases/tag/v0.2.0
