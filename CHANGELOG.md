# Changelog

Track notable changes to CodeSage here.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and releases follow [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps stay backwards-compatible within the same minor line. Standard SemVer applies at 1.0.0.

## [Unreleased]

### Added

- Go language support. Parses functions, methods (with pointer and value receivers), structs, interfaces, type aliases, and constants. Qualified names use `ReceiverType.MethodName` convention. References track imports and function/method calls. Test discovery already recognized `_test.go` convention from v0.3.0; now the parser can index Go source files.

### Fixed

- MCP server no longer dies on mutex poisoning. The per-server caches and per-instance Embedder/Reranker locks use `parking_lot::Mutex`, which has no poison state, so a panicked handler can no longer take the whole stdio transport with it.
- MCP `search` and `export_context` no longer silently degrade when the configured reranker fails to load. Reranker init errors now propagate to the agent as structured errors with model/device context instead of being swallowed.
- `Reranker::new` now bails when GPU is requested but the binary was built without the `cuda` feature, matching `Embedder::new`. Previously fell through to CPU silently.
- MCP error responses now include the full anyhow cause chain (`{:#}`) so root causes survive across tool boundaries.
- DB row decoders no longer silently drop rows on schema drift or type mismatch. All 15 `filter_map(|r| r.ok())` / `flatten()` sites in `storage::Database` now propagate `rusqlite::Error`; unknown `SymbolKind` / `ReferenceKind` values surface as typed conversion errors instead of being relabelled as `Function` / `Call`.
- Malformed `.codesage/config.toml` is now a hard error instead of a silent revert to defaults (MiniLM/CPU/empty excludes). `load_project_config` and MCP `load_embedding_config` return `Result` and surface parse errors with path context.
- `parse_log` drops commits with unparseable timestamps and changes with unparseable add/delete counts instead of fabricating zeros that were indistinguishable from legitimate `ts=0` or zero-line changes. Reports a one-line summary when any lines were dropped.
- `is_ancestor` now distinguishes spawn failure (propagated) from exit-1 / exit-128 (fall back to full rescan with a warning). A broken `git` install is no longer silently hidden as "rebased history".
- `install-hooks` now propagates `current_exe()` failures instead of silently writing a hook that embeds the literal `codesage` path.
- `assess_risk` no longer swallows errors from internal `impact_analysis` and `test_sibling_exists`. A DB failure during risk computation surfaces with context instead of silently lowering the score.
- Graph query pipeline (`extract_known_symbols`, `annotate_with_symbols`, `impact_analysis`, `export_context`, `export_context_for_symbol`, `add_related_from_file`) propagates internal DB errors instead of converting them into "no matches" / "no symbols".
- `churn_percentile` now distinguishes "no git row" (Ok(0.0)) from DB error (Err), so a broken git-index doesn't silently lower risk scores.

### Changed

- Orphan-file cleanup (structural + semantic indexers) now runs inside a single transaction instead of one autocommit per deleted file. On a post-rename or mass-delete index this removes N fsyncs.
- Incremental indexing (structural + semantic) replaces its N+1 `get_file_hash` pattern with a single-query `all_file_hashes` preload. On 25k-file repos this cuts thousands of SQL round-trips to one sequential scan.
- Incremental git-history indexing preloads `git_co_changes` pair keys once (`all_co_change_pairs`) instead of running `SELECT COUNT(*)` per delta pair inside the write transaction. Uses `HashMap<String, HashSet<String>>` so membership probes are alloc-free.
- Search boost path now calls `symbol_exists` (LIMIT 1 probe) instead of `find_symbols` when all it needs is non-emptiness for a token.
- `annotate_with_symbols` batches its per-file symbol lookup via `symbols_for_files`, replacing N queries with one multi-path query. Same batching applied to semantic chunk augmentation during indexing.
- Tree-sitter symbol and reference queries now compile once per language into `LazyLock<QuerySpec>` statics (with cached capture indices) instead of being recompiled on every file parse. On a 25k-file repo this removes 50k `Query::new` invocations and their `capture_index_for_name` scans.
- `Database::insert_chunks` takes `&[(&str, u32, u32, &[f32])]` instead of `&[(String, u32, u32, Vec<f32>)]`. Semantic indexing stops cloning chunk text and embedding vectors just to satisfy the API; on repo-sized batches this saves hundreds of megabytes of redundant allocation.
- `embedding_to_bytes` uses a single memcpy on little-endian targets instead of per-element `to_le_bytes`. The function is hit once per chunk write and once per search query.
- Reranker `score_batch` stops building an intermediate `Vec<(String, String)>` for tokenizer pairs; it now borrows query + doc refs directly.
- `Database::remove_file` now cascades the deletion to `git_files` and `git_co_changes`. Deleted files no longer linger in `find_coupling` / `assess_risk` output after an index refresh.
- Schema migrations now live in a registry (`MIGRATIONS` + `schema_migrations` table) rather than as hand-wired function calls inside `init_db`. Adding migration `0002`, `0003`, ... is now a one-line addition. The first migration (`0001_refs_name_tail`) is recorded on both fresh and legacy databases so subsequent init calls are no-ops.
- `crates/graph/src/git_history.rs` (1149 lines) split into `git_history/{mod.rs, indexer.rs, risk.rs, tests_rec.rs}`. Public API unchanged.
- `crates/storage/src/db.rs` (1230 lines) split into `db/{mod.rs, structural.rs, semantic.rs, git_hist.rs}` via `impl Database` blocks per concern. Public API unchanged.
- MCP tool responses now use the structured `CallToolResult` shape (`isError: true` on failure per MCP spec, `structured_content` on success alongside the pretty-printed text). MCP clients can programmatically distinguish errors from successful results instead of regexing "Error: " strings.
- `ImpactTarget::from_hint` and `ExportRequest::from_target` in the protocol crate centralize the CLI↔MCP classification/construction helpers. Previously duplicated (with slight shape differences) between `cli::main` and `cli::mcp`.
- CLI + library crates now log via the `tracing` crate instead of raw `eprintln!`. `tracing_subscriber` initializes at binary startup writing to stderr (keeps stdout clean for the MCP transport and CLI JSON output). Log level follows `RUST_LOG`, defaults to `info`.
- Hand-rolled `DistRow` newtype + BinaryHeap in the multi-language search merge path replaced by a `sort_by` on the merged vec. No behavior change.
- Duplicated `format_bytes` / `git_common_dir` helpers in `cli::main` and `cli::doctor` pulled into `cli::util`. `format_bytes` now uses GiB/MiB/KiB consistently (doctor previously reported GB/MB/KB powers-of-1024 with different labels).

## [0.3.3] - 2026-04-15

Test discovery for Laravel/Symfony mirror-tree projects + test backfill.

### Added

- `recommend_tests` now finds tests for Laravel mirror-tree layouts. Source at `app/<rest>/<file>.php` resolves to tests at `tests/{Unit,Feature,Integration,Browser}/<rest>/<file>Test.php`. Previously only flat `tests/Unit/FooTest.php` was checked, missing tests like `tests/Integration/Actions/CredentialingApplication/ExportZipActionTest.php`.
- `recommend_tests` now finds tests for Symfony mirror-tree layouts. Source at `src/<rest>/<file>.php` resolves to test at `tests/<rest>/<file>Test.php` (no Unit/Feature subdir; tests/ mirrors src/ directly).

### Fixed

- Inline unit tests for `accumulate()` cover the v0.3.1 source↔test pair filter (previously verified only via integration). Pins down the rule: source-test pairs are kept; test-test pairs are skipped.

[0.3.3]: https://github.com/iliaal/codesage/releases/tag/v0.3.3

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
