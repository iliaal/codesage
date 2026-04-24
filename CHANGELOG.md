# Changelog

Track notable changes to CodeSage here.

The format follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/), and releases follow [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps stay backwards-compatible within the same minor line. Standard SemVer applies at 1.0.0.

## [Unreleased]

### Added

- `assess_risk_diff` now reports import cycles that involve patch files. New `cycles_touching_patch[]` field on `RiskDiffAssessment` lists each strongly-connected component of size ≥2 in the file-level import graph where at least one member is in the patch's file list. Each entry carries `members` (sorted repo-relative paths), `size`, and `max_churn_file` (highest-churn member — the "most likely refactor site" heuristic; often the right place to break the cycle). When non-empty, a summary note like `"2 import cycle(s) involve patch files (largest: 3 files)"` lands in `summary_notes` for PR-description inclusion. Edges are derived from the existing `refs` table (kind = import / include / inheritance / trait_use resolving through `symbols.name` or `symbols.qualified_name`), so no new data collection. Uses an iterative Tarjan's SCC so deep include chains (php-src-scale) don't blow the stack. Honest caveat documented in the MCP tool description: we detect cycles the patch *touches*, not cycles the patch *introduced* — we don't have a pre-patch index to diff against, so pre-existing cycles involving patched files get reported too. Recommendations doc §1.1 (source: SocratiCode `codebase_graph_circular`, GitNexus cycle detection). 4 new tests cover two-file cycles, three-file diamonds, patches outside the cycle (not reported), and linear acyclic graphs.
- Advisory file-lock coordination for concurrent `codesage` writers. `index`, `git-index`, and `cleanup` now take an exclusive `flock` on `.codesage/indexing.lock` at startup (stdlib `File::try_lock`, no new dep). If another writer already holds it, the second process exits 0 with `another codesage indexer is running on <path> — skipping` instead of colliding at the SQLite layer and emitting a scary `Error: database is locked`. Matters most for `install-hooks`-registered background hooks (`post-commit`, `post-merge`, `post-checkout`, `post-rewrite`), where rapid commits or a `git pull` during a manual index used to spam hook logs with SQLite contention errors. The concurrency-audit finding from recommendations doc §2.4 confirmed no data was at risk, but the UX looked like a bug; this eliminates the class of error. Re-run `bench/concurrency-audit.py` on any onboarded project to verify.
- Gated hybrid BM25+semantic retrieval (recommendations doc §2.1 accepted). New FTS5 sidecar tables `{chunk_table}_fts` are created alongside each vec0 chunk table and populated in lockstep during `insert_chunks`. On `search`, a gate function (`query_has_rare_literal`) decides whether the query contains a distinctive literal — backticked identifiers, `::` scope resolution, `*.ext` globs, dotted-identifier pairs like `moduleref.create`, or long (≥8 char) code-shaped tokens with <1% corpus DF. When the gate fires, BM25 results are fetched from the FTS5 sidecar and fused with the semantic ranking via weighted Reciprocal Rank Fusion (BM25 weighted 2x). The cross-encoder reranker is skipped in the gated path because it consistently demotes literal-match wins (the exact failure mode the original `project_hybrid_bm25_rrf.md` memo warned about). Rare-literal-shaped queries that miss from plain semantic retrieval (the memo's predicted failure mode on mechanism-heavy external corpora) are the specific target; everything else keeps the semantic-only path unchanged. Measured on canary corpora: ripgrep miss@10 20% → 13% (−7pp), nest miss@10 10% → 3% (−7pp), zero regressions. (MCP and CLI) now returns `CouplingReport { coupled, file_indexed, file_commits, note? }` instead of a bare `Vec<CoChangeEntry>`. Agents should read `response.coupled` for the ranked list. Motivation: retrospective session analysis showed 59% of `find_coupling` calls returned `[]` with no context, leaving the agent unable to tell apart the three causes — file never indexed, file has history but no co-change pair crosses the min-count=3 threshold, or path shape doesn't match the index. The new `note` field disambiguates each case with a concrete hint (run `codesage git-index`, verify path shape, or accept that this file changes in isolation). `file_indexed` and `file_commits` are always present, so even non-empty results carry enough context for the agent to judge a thin response.

### Fixed

- MCP tool params now accept integer fields encoded as JSON strings (`"limit": "5"` alongside `"limit": 5`). Strict `Option<usize>` deserialization was failing with `invalid type: string "5", expected usize` on ~10% of `find_coupling` calls; retrospective session-log analysis (`bench/analyze-codesage-quality.py`) found agents occasionally emit stringy numbers, which is standard LLM JSON behavior and not something the protocol should reject. Applies uniformly to `limit`, `offset`, and `depth` across `CouplingParams`, `ImpactParams`, `ExportContextParams`, and `SearchParams`. Genuinely non-numeric strings still error, and the error now quotes the offending value for diagnosability instead of a generic type-mismatch message.

### Added

- `assess_risk_diff` now clusters per-file detail when a patch touches ≥5 files from one directory. The crowded directory's entries move from `files[]` into a new top-level `clustered_directories[]` field (top-3 files by score preserved in full detail, the rest listed by name as `omitted_files`). Rollup arrays (`test_gap_files`, `wide_blast_files`, `fix_heavy_files`, `hotspot_files`) still list every clustered file, so cross-referencing a cluster back to a specific concern still works. Small patches (≤4 files per directory) keep the original flat shape, so agent prompts written against the prior schema keep working without changes. Addresses recommendations doc §1.5 after retrospective session-log analysis measured this as the real cost center (p95 24 KB responses on this tool alone).
- Stronger MCP tool descriptions for `find_symbol`, `find_references`, and `search` that explicitly say "prefer over Grep for code-symbol lookups" and call out the specific disambiguation failure modes (grep mixing definitions, comments, and string literals). Session-log analysis showed the agent was reflexively reaching for Grep on patterns codesage answers in one call; sharper descriptions are the first intervention.

### Fixed

- `codesage status` no longer errors with `no such table:` on projects opened without a selected embedding model. Root cause: `cmd_status` opened the DB via `Database::open()` (which leaves `chunk_table` empty) and then called `chunk_count()`, which interpolated the empty table name into its SQL. Status now calls a new `total_chunk_count()` that sums across every vec0 chunk table in the DB — a more useful number anyway, since DBs that have been benchmarked across multiple models carry orphan chunk tables cleanup hasn't dropped. Zero vec tables returns 0 instead of failing.

### Added

- Drift instrumentation for the structural/semantic index. New `structural_index_state` table (migration `0002`) records the HEAD SHA each successful `codesage index` run built against. `codesage status` and `codesage doctor` now show an `index-drift` indicator (`fresh`, `N commits behind HEAD`, `not an ancestor of HEAD` for rebases, or `never indexed`). The MCP server appends one JSON line to `<project>/.codesage/drift.log` the first time it resolves a project in a session — bounded to the last 10,000 lines. No auto-reindex behavior: this is pure measurement so we can characterize how often git-hook drift happens in real usage before deciding whether to build the content-hash backstop (recommendations doc §1.3).

### Changed

- `bench/codesage-bench-runner` scorecard output now includes a full metadata header: project HEAD SHA, repo size (file + LoC counts), corpus + case count + top-N, codesage version, embedding model + device, reranker, explicit baseline-for-comparison slot, and ISO run timestamp. Adds a "measured quantities" / "NOT measured" block so the honesty gap (no agent tool-call counts, no wall-clock-vs-grep) is visible in the output itself. New trailing "Quotable one-liner" copies all metadata into a single sentence suitable for external communication; the machine-parseable `METRICS` HTML comment gains the new fields for downstream tooling. Optional `--baseline "ripgrep 14.1.0 (grep -rn)"` flag (or `CODESAGE_BENCH_BASELINE` env) fills the baseline slot — informational only, does not run a comparison tool. Template shape follows landscape-sweep item 1.6 in `notes/20260424-reference-tool-recommendations.md`.

### Security

- Bump transitive `openssl` crate to 0.10.78 (from 0.10.77) to pick up fixes for CVE-2026-41676, CVE-2026-41677, CVE-2026-41678, CVE-2026-41681, and GHSA-hppc-g8h3-xhp3. Four of the five are high-severity memory-safety bugs in rust-openssl callbacks and AES key wrap. CodeSage pulls `openssl` transitively via `native-tls` (used by `hf-hub` for model downloads and `ort-sys` at build time), so the exposure is limited to TLS paths exercised during model fetches, but the bump is cheap and closes the Dependabot alerts.
- Bump transitive `rustls-webpki` to 0.103.13 (from 0.103.12) for RUSTSEC-2026-0104 (reachable panic in CRL parsing). Pulled via `rustls` → `ureq` / `hyper-rustls` / `tokio-rustls` on the same model-download and reqwest paths. Clears the scheduled `cargo audit` workflow failure.

## [0.4.0] - 2026-04-16

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

[0.4.0]: https://github.com/iliaal/codesage/releases/tag/v0.4.0

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
