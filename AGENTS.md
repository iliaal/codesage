# CodeSage

Code intelligence engine with structural graph and semantic search. Rust workspace, tree-sitter parsing, ONNX embedding inference, cross-encoder reranking, SQLite storage, MCP interface.

## Build

```bash
cargo build                                    # all crates
cargo build --release -p codesage --features cuda  # release binary with GPU
cargo test --workspace                         # all tests
cargo clippy --workspace                       # lint
```

> Always build with `--features cuda` when targeting GPU. The binary errors if GPU is requested in config but the `cuda` feature is missing; it does not silently fall back to CPU.

## Sanity check before pushing

Run `bash scripts/sanity-check.sh` before `git push` when you've made code changes. It checks changelog/plugin metadata, Rust formatting and clippy, shell lint/format, workspace tests, and script/plugin regressions, stopping on the first failure. Pass `--cuda` to include CUDA clippy; the release workflow requires it. `--fast` skips workspace tests and script/plugin regressions while retaining the validation and lint gates; do not use it for releases.

`cargo fmt --all` applies changes in place; CI runs `cargo fmt --all -- --check`, which only reports the diff and exits nonzero. The script catches an edit made after the last format run (as in commit `a43c51d`) before CI does. Run `cargo fmt --all` after the last hand edit and before the script: its first gate is the format check, and any diff stops the chain before clippy and tests run.

## Crate map

| Crate | Role | Depends on |
|-------|------|------------|
| `protocol` | Shared types (Symbol, Reference, SearchResult, etc.) | nothing |
| `parser` | File discovery, language detection, tree-sitter symbol/reference extraction | protocol |
| `storage` | SQLite schema, CRUD, sqlite-vec KNN | protocol |
| `embed` | ONNX embedding inference (Embedder), cross-encoder reranking (Reranker), chunking | protocol, ort, tokenizers, hf-hub |
| `features` | Feature-slice mapping, trust-boundary derivation | parser, storage, protocol |
| `graph` | Indexing orchestration, search pipeline, query API | parser, storage, embed, features, protocol |
| `cli` | `codesage` binary: CLI subcommands + MCP stdio shim + Unix-socket daemon | everything |

## Search pipeline

Query flows through these stages in order:

1. **Embed query**: Jina embeddings v2 base-code (768d) via ONNX Runtime
2. **KNN retrieval**: sqlite-vec, overfetch 5x when the reranker is active
3. **BM25 fusion**: code-literal queries (backticks, `::`, glob patterns) merge BM25 candidates via RRF; fused scores are rescaled onto the semantic score span so downstream boosts can't swamp them
4. **Symbol boost**: +0.1 per query token that matches a known symbol in the chunk, plus definition/qualified-name boosts, path penalties, and directory/file saturation (each env-toggleable)
5. **Symbol annotation**: attach overlapping symbol names to each result
6. **Cross-encoder rerank**: ms-marco-MiniLM-L6-v2, adaptive blend weight (0.35 identifier-shaped / 0.6 natural-language / 0.5 default); skipped when BM25 fusion already ran
7. **Query-mention anchoring**: a path, dotted module, or `Type::method` named outright in the query lifts its matching rows onto a slot ladder directly under the top score (each anchored row ends at or above `top * 0.95^(slot+1)`; at most 5 rows, 4 files x 3 chunks, first page only). A path suffix that matches several indexed files anchors nothing. Inert when the query names nothing; disable with `CODESAGE_MENTION_ANCHOR=0`.
8. **Truncate** to requested limit
9. **Relevance-cliff disclosure**: the page reports `confidence` (`high` when the largest adjacent relative score drop rounds to ≥20%, else `low`), `margin_pct`, and `cliff_at`. Opt-in `adaptive_limit: true` (CLI `--adaptive-limit`) cuts the page at that drop when `confidence` is `high`; a flat page is returned in full. The cut is page-local, so it composes poorly with `offset` paging.

The reranker is optional (configured per-project in config.toml). Without it, the remaining stages still run.

## Config

Per-project config lives at `.codesage/config.toml`:

```toml
[project]
name = "my-project"

[embedding]
model = "jinaai/jina-embeddings-v2-base-code"
device = "gpu"
reranker = "cross-encoder/ms-marco-MiniLM-L6-v2"

[index]
exclude_patterns = []

[docs]
exclude_patterns = ["docs/spikes/**"]
```

Built-in exclusions cover dependencies, build outputs, and caches; configured patterns add to them. Tests are indexed structurally and semantically by default, then demoted during search. Explicitly excluding tests reduces graph-based test discovery and test-gap evidence. The optional `[docs]` example above controls documentation checks, not indexing.

`[embedding] model` / `reranker` values are validated against a built-in allowlist before any download, then loaded only from pinned Hugging Face revisions with sha256-verified tokenizer/ONNX artifacts, because repo-local config is untrusted input (a cloned repo must not be able to pick the graph that gets loaded). To run a non-allowlisted or unpinned model deliberately, set `CODESAGE_ALLOW_ANY_MODEL=1`; the allowlist and pins live in `crates/embed/src/model.rs`. A pin may fetch from a repository other than the configured name and may carry a second graph used only when `device` selects CUDA: `jinaai/jina-embeddings-v2-base-code` loads `IA0x00/jina-embeddings-v2-base-code-codesage`, whose `onnx/model.onnx` (fp32, pooling and L2 normalization in-graph) and `onnx/model_cuda_fp16.onnx` (fused `MultiHeadAttention`, fp16) are rebuilt byte-for-byte by `scripts/derive-jina-onnx.py`; the `cross-encoder/ms-marco-MiniLM-L6-v2` reranker loads `IA0x00/ms-marco-MiniLM-L6-v2-codesage` (upstream graph plus an ORT-fused fp16 CUDA graph from `scripts/derive-reranker-onnx.py`). The reranker truncates pairs at 512 tokens, its position-embedding limit, whatever the embedder's cap. A graph that emits `[batch, dim]` is taken as already mean-pooled, so configuring `pooling = "cls"` for it is an error.

## CUDA setup

ONNX Runtime loads dynamically. CUDA libraries come from pip-installed `nvidia-*-cu12` packages. At first use, the binary discovers them in this order:

1. `CODESAGE_NVIDIA_LIBS` env var, if set (an explicit nvidia root directory).
2. Python `site.getsitepackages()` + `site.getusersitepackages()`, joined with `/nvidia`. Works with both system-wide pip installs and `--user` installs.
3. Standard system paths: `/usr/lib/x86_64-linux-gnu/nvidia`, `/usr/local/lib/nvidia`, `/opt/nvidia`.

`ORT_DYLIB_PATH` can override the ONNX Runtime library location. Left unset, the binary probes the same site-packages locations for `libonnxruntime.so*`.

`codesage doctor` reports how many nvidia lib dirs were discovered and warns if none.

If CUDA is requested (`device = "gpu"`) but fails to register, the process errors out instead of falling back to CPU. This is intentional: silent CPU fallback produces different embeddings and slower performance. The check inspects `/proc/self/maps` after session creation and hard-fails when libcuda/libcudart aren't mapped. To bypass the Linux post-session library check deliberately (tests, mixed CPU/GPU setups), set `CODESAGE_ALLOW_CPU_FALLBACK=1`. If that check fails with the override enabled, the session logs a warning and fingerprints its execution provider as CPU; do not leave the override set on a GPU-configured project.

Required pip packages: `onnxruntime-gpu`, `nvidia-cudnn-cu12`, `nvidia-cublas-cu12`, `nvidia-cuda-runtime-cu12`, `nvidia-cufft-cu12`, `nvidia-curand-cu12`, `nvidia-cuda-nvrtc-cu12`.

## CoreML setup (macOS)

On Apple Silicon, set `device = "coreml"` in `.codesage/config.toml`. macOS builds statically link ONNX Runtime with the CoreML EP at compile time (`ort` `coreml` feature via target-specific deps in `crates/embed/Cargo.toml`); Linux/CUDA keeps `load-dynamic`. First session creation compiles CoreML submodels and can take a few minutes; subsequent runs in the same process are faster. Large models (e.g. Jina v2 base-code) may need a lower embed batch size than the default `BATCH_SIZE` in `crates/embed/src/config.rs` if memory pressure causes OOM during indexing.

If CoreML registration fails, the process errors out instead of silently falling back to CPU.

## Conventions

- Rust 2024 edition
- `anyhow` for error handling workspace-wide; shared domain types live in the `protocol` crate
- Tree-sitter queries in `.scm` files under `crates/parser/src/queries/`, embedded via `include_str!`
- JSON output on all query commands (`--json`)
- Model-specific vec0 tables (`chunks_{model}_{dim}`) allow switching models without re-indexing structural data
- Symbol patterns in `crates/parser/src/queries/*.scm` dispatch on `pattern_index`, so each file is positionally coupled to its language's `kind_map` in `crates/parser/src/extract.rs`. Append a new pattern at the end, add its ordinal arm, and bump `EXPECTED_SYMBOL_PATTERN_COUNTS` in `crates/parser/src/validate.rs`; inserting mid-file silently changes the kind of every later pattern. Verify with `cargo test -p codesage-parser`.
- ONNX inference paths (`crates/embed/src/model.rs`, `crates/embed/src/reranker.rs`) index a flat output by a shape-derived stride. Detect a squeezed rank-1 `[batch]` output with `shape.len() <= 1`, not `shape.last()`, and check `len == batch * stride` before indexing; a panic inside a daemon tool handler leaves the MCP client waiting with no reply.
- Wrap any loop that writes more than a handful of rows in `Database::execute_batch` (`crates/storage/src/db/mod.rs`). SQLite otherwise autocommits each statement, and per-row fsyncs stalled php-src history indexing until v0.3.2.
- Breadth metrics over the feature map (how many areas a patch touches) key on the feature's entry directory (`entry_area` in `crates/graph/src/rehearsal.rs`), not `feature_id`: the php-src mapper emits one feature per PHP function, so a single file can carry 60+ feature ids.

## Versioning and changelog

This repo follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html). Workspace version lives in `[workspace.package] version` in the root `Cargo.toml`; all seven crates inherit it via `version.workspace = true`.

**Every release-notable product change must update `CHANGELOG.md` in the same commit.** Release-notable means: new CLI flags or subcommands, new or changed MCP tools, behavior changes, breaking changes, new dependencies, schema migrations, hook template changes, config surface changes, and security fixes that affect shipped CodeSage behavior.

No changelog entry for pure internal refactors, tests, benchmark/eval harnesses, review-process fixes, doc-only changes, or performance-only internals whose output and operator contract are unchanged.

Write entries in terse style:

- Put entries under `## [Unreleased]` in these sections, in this fixed order: `Added` → `Changed` → `Deprecated` → `Removed` → `Fixed` → `Security`. This is the shared iliaal/* Keep-a-Changelog section ordering.
- Skip empty subsections; never carry a placeholder bullet just to populate the structure. Any project-specific section (none today) comes after the standard ones.
- Use one plain bullet per user-visible change. No bold lead-in, no paragraph explanation, no file lists.
- Name the command, MCP tool, config key, or behavior that changed. Stop after the observable effect.
- Prefer consolidation when several fixes share one surface (`codesage daemon status` / `stop`, parser symbol extraction, feature mapping).
- If a reviewer would need the git diff to care, it probably does not belong in the changelog.

`scripts/check-changelog.py` enforces the section set, ordering, and no-empty-section rules on `## [Unreleased]`; it runs as a release pre-flight (`scripts/release.sh` and `/release`). Run it directly any time: `python3 scripts/check-changelog.py`.

### Cutting a release

Use the repository-owned `/release X.Y.Z` workflow and its canonical implementation, `scripts/release.sh X.Y.Z`, rather than manually bumping Cargo or creating a tag.

The script rolls over the changelog, updates the workspace version, both Codex and Claude plugin manifests, and the marketplace metadata and plugin entry. It runs `scripts/check-changelog.py` and `scripts/check-plugin-versions.py` before mutation, then repeats version alignment validation after mutation and before committing/tagging. Keep that gate in the release path; the tag-triggered workflow is not a substitute.

For a non-publishing alignment check on a synthetic release fixture, run `python3 scripts/check-plugin-versions.py --root /path/to/fixture`; do not invoke the publishing release script for that check.

The `Release` workflow (`.github/workflows/release.yml`) fires on the tag push, extracts the matching `[X.Y.Z]` section from `CHANGELOG.md`, and creates a GitHub Release with those notes plus the auto-attached source tarball. If the section is empty or missing, the workflow fails.

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps are backwards-compatible within a minor line.

## Behavior reference

`docs/reference.md` holds the detailed behavior contract: per-language extraction rules (C/C++ macro neutralization, lazy imports, visibility, `is_test`, Rust module and cross-crate resolution), every MCP tool and the shared response envelope, the daemon runtime, CLI commands, git history intelligence, and feature mapping with trust boundaries. Read the matching section before changing one of those surfaces, and update it in the same commit as the behavior change. `codesage doctor --docs` checks it along with the rest of `docs/`.

Supported languages: PHP, Python, C, C++, Java, Rust, JavaScript, TypeScript, Go.

- Parser, extraction, and trust-boundary rule changes bump the relevant component of `STRUCTURAL_INTERPRETATION` in `crates/graph/src/index.rs`.
- Mapper output changes for unchanged input bump `MAPPER_OUTPUT_VERSION` in `crates/features/src/lib.rs`.
- A new CLI subcommand also goes into the command list in `docs/reference.md`, which mirrors the `Commands` enum in `crates/cli/src/main.rs`.

## Benchmarks

Benchmark harness under `bench/` (curated examples; see `bench/` for the full inventory, including ablation, semble-corpus, concurrency-audit, agent-task, and quality-analysis runners):

- `bench/codesage-bench-runner`: Python runner that executes a YAML corpus of ground-truth cases against `codesage search` and reports miss rate, median first-hit, recall@5, recall@10.
- `bench/extract-eval-cases.py`: mines eval cases from Claude Code session transcripts and git commit history.
- `bench/cleanup-orphan-models.sh`: drops orphaned vec tables from prior model switches.

Corpus YAMLs are not bundled; bring your own. `CODESAGE_BENCH_CORPUS_DIR` (consumed by `/codesage-bench` and `/codesage-eval` plugin commands) points the plugin at the directory holding them.

## Plugin

`plugins/codesage-tools/` ships as a Claude Code plugin: one global `codesage` MCP registration serves every onboarded project, routed by an absolute `project` argument. The registered command remains `codesage mcp`; the shim handles daemon startup and reuse. Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`, `/codesage-prompt-override`, and the four feature-slice review commands (`/codesage-review`, `/codesage-triage`, `/codesage-revalidate`, `/codesage-report`). Marketplace manifest at repo root.
