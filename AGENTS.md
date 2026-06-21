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

## Sanity check before pushing

Run `bash scripts/sanity-check.sh` before `git push` when you've made code changes. Chains `cargo fmt --all -- --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`, in that order, stopping on the first failure. Pass `--fast` to skip tests (CI runs them) when you just want the fmt/clippy gate.

The "fmt then edit then forget to re-fmt" class of break is real (commit `a43c51d` is its monument); `cargo fmt --all` applies changes in place, but `cargo fmt --all -- --check` only reports the diff and exits nonzero — CI runs the latter. Using the script means you catch it locally.

## Crate map

| Crate | Role | Depends on |
|-------|------|------------|
| `protocol` | Shared types (Symbol, Reference, SearchResult, etc.) | nothing |
| `parser` | File discovery, language detection, tree-sitter symbol/reference extraction | protocol |
| `storage` | SQLite schema, CRUD, sqlite-vec KNN | protocol |
| `embed` | ONNX embedding inference (Embedder), cross-encoder reranking (Reranker), chunking | ort, tokenizers, hf-hub |
| `graph` | Indexing orchestration, search pipeline, query API | parser, storage, embed, protocol |
| `cli` | `codesage` binary: CLI subcommands + MCP stdio shim + Unix-socket daemon | everything |

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

## CoreML setup (macOS)

On Apple Silicon, set `device = "coreml"` in `.codesage/config.toml`. macOS builds statically link ONNX Runtime with the CoreML EP at compile time (`ort` `coreml` feature via target-specific deps in `crates/embed/Cargo.toml`); Linux/CUDA keeps `load-dynamic`. First session creation compiles CoreML submodels and can take a few minutes; subsequent runs in the same process are faster. Large models (e.g. Jina v2 base-code) may need a lower embed batch size than the default `BATCH_SIZE` in `crates/embed/src/config.rs` if memory pressure causes OOM during indexing.

If CoreML registration fails, the process errors out instead of silently falling back to CPU.

## Conventions

- Rust 2024 edition
- `anyhow` in binaries, types in protocol crate
- Tree-sitter queries in `.scm` files under `crates/parser/src/queries/`, embedded via `include_str!`
- JSON output on all query commands (`--json`)
- Model-specific vec0 tables (`chunks_{model}_{dim}`) allow switching models without re-indexing structural data

## Versioning and changelog

This repo follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html). Workspace version lives in `[workspace.package] version` in the root `Cargo.toml`; all six crates inherit it via `version.workspace = true`.

**Every release-notable product change must update `CHANGELOG.md` in the same commit.** Release-notable means: new CLI flags or subcommands, new or changed MCP tools, behavior changes, breaking changes, new dependencies, schema migrations, hook template changes, config surface changes, and security fixes that affect shipped CodeSage behavior.

No changelog entry for pure internal refactors, tests, benchmark/eval harnesses, review-process fixes, doc-only changes, or performance-only internals whose output and operator contract are unchanged.

Write entries in terse style:

- Put entries under `## [Unreleased]` in these sections, in this fixed order: `Added` → `Changed` → `Deprecated` → `Removed` → `Fixed` → `Security`. This is the shared iliaal/* Keep-a-Changelog section ordering (see `~/ai/wiki/architecture/php-extension-c-conventions.md` § CHANGELOG section ordering).
- Skip empty subsections; never carry a placeholder bullet just to populate the structure. Any project-specific section (none today) comes after the standard ones.
- Use one plain bullet per user-visible change. No bold lead-in, no paragraph explanation, no file lists.
- Name the command, MCP tool, config key, or behavior that changed. Stop after the observable effect.
- Prefer consolidation when several fixes share one surface (`codesage daemon status` / `stop`, parser symbol extraction, feature mapping).
- If a reviewer would need the git diff to care, it probably does not belong in the changelog.

`scripts/check-changelog.py` enforces the section set, ordering, and no-empty-section rules on `## [Unreleased]`; it runs as a release pre-flight (`scripts/release.sh` and `/release`). Run it directly any time: `python3 scripts/check-changelog.py`.

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

PHP, Python, C, C++, Java, Rust, JavaScript, TypeScript, Go.

`.h` files default to C. The discovery layer auto-flips them to C++ for any project that also contains an unambiguous C++ extension (`.cpp`, `.cc`, `.cxx`, `.hpp`, etc.). `.c` always stays C. No config knob — if you need to override on a project that mixes both styles awkwardly, raise an issue.

## MCP tools

- `project_overview` -- one bounded first-call orientation: languages, structural + semantic freshness, feature summary by kind, top-risk files, trust-boundary clusters, per-language test conventions, sample entrypoints, and suggested next calls. Pure aggregation over the index; call once at session start.
- `search` -- semantic search with embedding + reranking
- `find_symbol` -- symbol definitions by name
- `find_references` -- references to a symbol; each row's `from_symbol` names the enclosing caller (null at file scope)
- `find_similar` -- near-clone detection: functions/methods structurally similar to a named one (MinHash over AST shape, identifiers/literals ignored), ranked by Jaccard. Test files excluded. Needs fingerprints from a reindex.
- `list_dependencies` -- file-level imports/imported-by
- `impact_analysis` -- files affected by changing a symbol or file, with distance and reasons. Opt-in `include_forward` (forward deps), `include_siblings` (same-file symbols), `limit`, and `summary_only` controls; result is an object with `results` plus the requested extras.
- `export_context` -- curated code bundle for a query or symbol, optionally with callers/callees
- `find_coupling` -- files that historically change with a given file (V2b)
- `assess_risk` -- risk score for a single file (V2b slice 1; now blends import-cycle membership alongside churn / fix / blast / coupling / test-gap)
- `assess_risk_batch` -- per-file risk for N files in one call, no aggregation. Use when you have a list of files and want each one's score; saves the per-file MCP round-trip overhead vs N `assess_risk` calls. For patch-level aggregation use `assess_risk_diff` instead.
- `assess_risk_diff` -- aggregate risk for a patch / set of files (V2b slice 2). Per-file `notes[]` may contain short codes (`"T"`, `"NG"`); resolve via the top-level `_legend` map.
- `recommend_tests` -- tests an agent should run after editing a set of files (V2b slice 2)
- `review_rehearsal` -- predict severity-ranked review objections for a patch (missing tests, high-risk / blast-radius / fix-prone / hotspot files, import cycles, trust-boundary expansion, feature-test gaps, and `scope-spread` when a patch touches ≥4 unrelated feature areas) with hot-symbol evidence. Composes `assess_risk_diff` + `recommend_tests` + drift + feature mapping; read-only, no AI prose. Use as the last step before a commit.
- `session_start` / `session_end` -- snapshot structural state at the start of an editing session, diff at the end. Returns `pass: bool` plus new/resolved cycles, per-file risk regressions on the top-50 baseline, and added/removed files.
- `list_features` -- list mapped feature slices, filterable by `kind` (`route`, `cli-command`, `library`, `test-suite`, `service`, `config`, `infra`), `language`, or `tag` (0.7.0).
- `find_feature` -- given a file path, return the feature(s) that own it. Routes "what slice owns this file?" without scanning by hand.
- `feature_bundle` -- curated code bundle for one feature slice (entry + owned + tests + context as primary/related chunks, plus the entry symbol's definition and optionally its callers/callees). Same shape as `export_context` but anchored on the feature's pre-curated file list. Returns `not found` marker when the `feature_id` is unknown.

Every MCP tool advertises an `outputSchema` (0.7.0); agents that consult it know the result shape before they call.

## MCP runtime

`codesage mcp` is the stable client entrypoint. It runs as a stdio shim, starts or connects to the per-user Unix-socket daemon, and forwards MCP JSON-RPC unchanged. The daemon hosts the real MCP server and owns shared project/model/reranker pools across main sessions and subagents.

`codesage mcp --project <abs root>` makes the server default the per-call `project` argument to that root when a `tools/call` omits it. Set automatically by `codesage install` for agents without a CodeSage plugin (Codex, opencode), which otherwise have no way to inject the project path. With no `--project` (the Claude-plugin path) the shim raw-copies stdio with zero overhead.

Use `codesage mcp --direct` only when debugging the old single-process stdio path. Use `codesage daemon` to run the foreground daemon explicitly. Socket state lives under `$CODESAGE_DAEMON_RUNTIME_DIR`, `$XDG_RUNTIME_DIR/codesage`, or `/tmp/codesage-$UID`; the socket name includes the running binary's version and executable metadata so rebuilt binaries don't attach to stale daemons.

### Daemon management

- `codesage daemon` runs the daemon in the foreground (the default action).
- `codesage daemon status` prints the running daemon's pid, socket path, and log path; exit 1 if not running.
- `codesage daemon stop` sends SIGTERM, waits up to 10s for cleanup, and reports.

Runtime files per daemon: `mcp-<version>-<key>.sock` (Unix socket, 0o600), `mcp-<version>-<key>.pid` (text pid), `mcp-<version>-<key>.lock` (start-lock during spawn), `mcp-<version>-<key>.log` (daemon stdout + stderr). The log is rotated when it crosses 4 MiB; three generations are retained (`.log`, `.log.1`, `.log.2`).

### Tracing

The daemon inherits the **first** spawning shim's environment, including `RUST_LOG`. Setting `RUST_LOG=codesage=debug` on the initial `codesage mcp` invocation that boots the daemon raises the daemon's log level for its entire lifetime; subsequent shims with different `RUST_LOG` values don't reconfigure the running daemon. To change filters mid-life, `codesage daemon stop` and let the next shim restart it under the new env.

The daemon writes tracing to `mcp-<version>-<key>.log` in the runtime dir; check that file first when a tool call hangs or an MCP session won't initialize. SIGTERM/SIGINT trigger graceful shutdown (socket + pid file removed before exit).

## CLI commands

`init`, `index`, `overview`, `search`, `find-symbol`, `find-references`, `dependencies`, `impact`, `export`, `status`, `mcp`, `daemon`, `watch`, `install-hooks`, `install`, `uninstall`, `cleanup`, `git-index`, `coupling`, `risk`, `risk-batch`, `risk-diff`, `similar`, `tests-for`, `rehearse`, `session-start`, `session-end`, `doctor`, `map`, `features-list`, `feature-show`, `feature-for`, `feature-bundle`, `trust-boundaries`.

`watch run|status|stop|start [project]` controls the live filesystem watcher. The daemon auto-starts a per-project watcher on the first MCP tool call for that project (reusing the daemon's pooled embedder), debounces edits, and reindexes structural + semantic on change; it self-exits after idle (`CODESAGE_WATCH_IDLE_SECS`) and is reaped on daemon shutdown. Disable per project with `[index] watch = false` or globally with `CODESAGE_WATCH=0`. `watch run` is a foreground instance with its own embedder for debugging; `watch stop` writes a `.codesage/watch.disabled` marker the running watcher honors; `watch start` clears it. The watcher complements the git hooks, it does not replace them: it refreshes structural + semantic content live during a session, but git history intelligence (`git-index`, feeding `assess_risk` / `find_coupling`) and feature mapping still refresh only via the hooks or a full `codesage index`, and the watcher only runs while a daemon is alive.

`install <codex|opencode|all> [--global]` registers CodeSage as an MCP server in agents that have no CodeSage plugin (Codex CLI, opencode), writing their native MCP config (`toml_edit` / `jsonc-parser` CST, comment-preserving and idempotent). It registers the command `codesage mcp --project <abs root>`; `uninstall` removes only CodeSage's entry. Claude Code is not a target — it keeps its `claude mcp add` / plugin registration.

`map` runs the feature mappers (Cargo workspace, composer + Laravel routes, php-src `ext/*`, CMake / autotools, Python `pyproject` / `setup.py` / `__main__`, `package.json` bin + Next.js routes, Go `cmd/*`) and persists features. `codesage index` calls `map` between the structural and semantic passes; `--no-features` skips. `features-list` / `feature-show` / `feature-for` / `feature-bundle` are read-side query commands matching the new MCP tools. `trust-boundaries <file>` is the debugging surface for the per-file boundary tags that feed `assess_risk`.

`cleanup` drops orphaned vec tables from previous model switches, keeping only the active model. Use after benchmarking multiple models. Runs VACUUM automatically.

## Benchmarks

Benchmark harness under `bench/`:

- `bench/codesage-bench-runner` — Python runner that executes a YAML corpus of ground-truth cases against `codesage search` and reports miss rate, median first-hit, recall@5, recall@10.
- `bench/extract-eval-cases.py` — mines eval cases from Claude Code session transcripts and git commit history.
- `bench/cleanup-orphan-models.sh` — drops orphaned vec tables from prior model switches.

Corpus YAMLs are not bundled; bring your own. `CODESAGE_BENCH_CORPUS_DIR` (consumed by `/codesage-bench` and `/codesage-eval` plugin commands) points the plugin at the directory holding them.

## Plugin

`plugins/codesage-tools/` ships as a Claude Code plugin: one global `codesage` MCP registration serves every onboarded project, routed by an absolute `project` argument. The registered command remains `codesage mcp`; the shim handles daemon startup and reuse. Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`. Marketplace manifest at repo root.

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

## Feature mapping + trust boundaries (shipped 0.7.0)

`crates/features/` runs after structural and before semantic indexing on every `codesage index`. It maps the project into **behavior-keyed slices** (entrypoint + owned files + context files + tests + aggregated trust boundaries + tags) and derives **per-file trust boundaries** from imports/includes/calls.

Mappers are deterministic (no LLM) and language-local:

| Mapper | Detects |
|---|---|
| Rust | `src/main.rs`, `src/bin/*.rs`, `src/lib.rs`, Cargo workspace members, `crates/*`, integration tests under `tests/*.rs` |
| PHP | `composer.json` bins + scripts, PSR-4 autoload roots, php-src `ext/*/config.{m4,w32}`, Laravel `routes/{web,api,console,channels}.php` |
| C / C++ | tree-sitter `main()` detection, `bin_PROGRAMS` / `lib_LTLIBRARIES` from autotools, `add_executable` / `add_library` from CMake |
| Python | `pyproject.toml [project.scripts]` (module-resolved entry path), `setup.py` `entry_points`, top-level `if __name__ == "__main__":` modules |
| JS / TS | `package.json` `bin` + selected scripts (`start`, `build`, `test`, `lint`, `typecheck`, `format`), Next.js `app/**` and `pages/**` routes |
| Go | `cmd/<name>/main.go` and a repo-root `main.go` when declared `package main` |

Tables (schema migration `0009_feature_tables`): `features`, `feature_files` (per-feature path×role refs), `feature_trust_boundaries` (per-feature boundary set).

Trust-boundary rule tables (`crates/features/src/trust_boundary_rules.rs`) cover Rust, PHP, C/C++, Python, Go, JavaScript/TypeScript plus Laravel facades; Java currently parses structurally without dedicated trust-boundary rules. Boundaries are: `network`, `filesystem`, `process-exec`, `secrets`, `database`, `user-input`, `external-api`, `serialization`, `auth`, `concurrency`. Per-file rows live in `file_trust_boundaries` (migration `0008`), with a `boundaries_derived_at` marker (migration `0010`) used by the indexer's targeted backfill to avoid re-running derivation on rule-clean files.

`assess_risk` consumes the per-file rows: `0.10 * min(boundary_count/5, 1.0)` adds to the composite score, capped at 5 boundaries. The `notes[]` line `"crosses N trust boundaries (X, Y, Z) — security review recommended"` fires when ≥3 boundaries are crossed. The signal lands in `RiskAssessment.trust_boundaries: Vec<TrustBoundary>`.

`[index].exclude_patterns` from the project's `.codesage/config.toml` are honored throughout the mapper crate via `MapperContext.excludes`. Mappers emit candidate seeds, the orchestrator filters entry paths and per-record file refs against the globset, so feature output matches the structural indexer's file-set contract.

The MCP surface — `list_features`, `find_feature`, `feature_bundle` — sits on top of these tables. The CLI surface — `map`, `features-list`, `feature-show`, `feature-for`, `feature-bundle`, `trust-boundaries` — mirrors it for terminal use.

## Roadmap

V1: semantic retrieval + structural graph + MCP interface, change impact analysis, context export, plugin-based deployment.

V2b slice 1 (shipped 0.2.0): git history intelligence — `find_coupling` + `assess_risk` MCP tools, `codesage git-index` CLI with incremental hooks.

V2b shipped (0.7.0): feature-slice mapping + trust-boundary derivation + `outputSchema` on every MCP tool. `crates/features/`, the `list_features` / `find_feature` / `feature_bundle` MCP tools, the per-language mappers, and the `file_trust_boundaries` signal feeding `assess_risk`. Ports the clawpatch (`openclaw/clawpatch`) feature-slice donor patterns into Rust.

V2b slice 2 (next): `bus_factor`, `change_pattern`, `find_hotspots` MCP tools. Conditional on slice 1 validating on large real codebases.

V2c (deferred): docs/decision layer (process traces, architecture summaries). Revisited after V2b slice 2 lands.
