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
| `embed` | ONNX embedding inference (Embedder), cross-encoder reranking (Reranker), chunking | protocol, ort, tokenizers, hf-hub |
| `features` | Feature-slice mapping, trust-boundary derivation | parser, storage, protocol |
| `graph` | Indexing orchestration, search pipeline, query API | parser, storage, embed, features, protocol |
| `cli` | `codesage` binary: CLI subcommands + MCP stdio shim + Unix-socket daemon | everything |

## Search pipeline

Query flows through these stages in order:

1. **Embed query** -- Jina embeddings v2 base-code (768d) via ONNX Runtime
2. **KNN retrieval** -- sqlite-vec, overfetch 5x when the reranker is active
3. **BM25 fusion** -- code-literal queries (backticks, `::`, glob patterns) merge BM25 candidates via RRF; fused scores are rescaled onto the semantic score span so downstream boosts can't swamp them
4. **Symbol boost** -- +0.1 per query token that matches a known symbol in the chunk, plus definition/qualified-name boosts, path penalties, and directory/file saturation (each env-toggleable)
5. **Symbol annotation** -- attach overlapping symbol names to each result
6. **Cross-encoder rerank** -- ms-marco-MiniLM-L6-v2, adaptive blend weight (0.35 identifier-shaped / 0.6 natural-language / 0.5 default); skipped when BM25 fusion already ran
7. **Query-mention anchoring** -- a path, dotted module, or `Type::method` named outright in the query lifts its matching rows onto a slot ladder directly under the top score (each anchored row ends at or above `top * 0.95^(slot+1)`; at most 5 rows, 4 files x 3 chunks, first page only). A path suffix that matches several indexed files anchors nothing. Inert when the query names nothing; disable with `CODESAGE_MENTION_ANCHOR=0`.
8. **Truncate** to requested limit
9. **Relevance-cliff disclosure** -- the page reports `confidence` (`high` when the largest adjacent relative score drop rounds to ≥20%, else `low`), `margin_pct`, and `cliff_at`. Opt-in `adaptive_limit: true` (CLI `--adaptive-limit`) cuts the page at that drop when `confidence` is `high`; a flat page is returned in full. The cut is page-local, so it composes poorly with `offset` paging.

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

`[embedding] model` / `reranker` values are validated against a built-in allowlist before any download, then loaded only from pinned Hugging Face revisions with sha256-verified tokenizer/ONNX artifacts — repo-local config is untrusted input (a cloned repo must not be able to pick the graph that gets loaded). To run a non-allowlisted or unpinned model deliberately, set `CODESAGE_ALLOW_ANY_MODEL=1`; the allowlist and pins live in `crates/embed/src/model.rs`.

## CUDA setup

ONNX Runtime loads dynamically. CUDA libraries come from pip-installed `nvidia-*-cu12` packages. At first use, the binary discovers them in this order:

1. `CODESAGE_NVIDIA_LIBS` env var, if set (an explicit nvidia root directory).
2. Python `site.getsitepackages()` + `site.getusersitepackages()`, joined with `/nvidia`. Works with both system-wide pip installs and `--user` installs.
3. Standard system paths: `/usr/lib/x86_64-linux-gnu/nvidia`, `/usr/local/lib/nvidia`, `/opt/nvidia`.

`ORT_DYLIB_PATH` can override the ONNX Runtime library location. Left unset, the binary probes the same site-packages locations for `libonnxruntime.so*`.

`codesage doctor` reports how many nvidia lib dirs were discovered and warns if none.

If CUDA is requested (`device = "gpu"`) but fails to register, the process errors out instead of falling back to CPU. This is intentional -- silent CPU fallback produces different embeddings and slower performance. The check inspects `/proc/self/maps` after session creation and hard-fails when libcuda/libcudart aren't mapped. To bypass it deliberately (tests, mixed CPU/GPU setups), set `CODESAGE_ALLOW_CPU_FALLBACK=1` -- the consequence is silently CPU-computed embeddings, so never leave it set on a GPU-configured project.

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

## Versioning and changelog

This repo follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and [SemVer 2.0.0](https://semver.org/spec/v2.0.0.html). Workspace version lives in `[workspace.package] version` in the root `Cargo.toml`; all seven crates inherit it via `version.workspace = true`.

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
3. Bump `[workspace.package] version` in the root `Cargo.toml`. All seven crates inherit it.
4. Commit: `git commit -am "release: vX.Y.Z"`.
5. Tag: `git tag -a vX.Y.Z -m "codesage X.Y.Z"`.
6. Push: `git push origin master && git push origin vX.Y.Z`.

The `Release` workflow (`.github/workflows/release.yml`) fires on the tag push, extracts the matching `[X.Y.Z]` section from `CHANGELOG.md`, and creates a GitHub Release with those notes plus the auto-attached source tarball. If the section is empty or missing, the workflow fails.

Pre-1.0 rule: minor bumps may include breaking changes, patch bumps are backwards-compatible within a minor line.

## Languages

PHP, Python, C, C++, Java, Rust, JavaScript, TypeScript, Go.

`.h` files default to C. The discovery layer auto-flips them to C++ for any project that also contains an unambiguous C++ extension (`.cpp`, `.cc`, `.cxx`, `.hpp`, etc.). `.c` always stays C. No config knob — if you need to override on a project that mixes both styles awkwardly, raise an issue.

Incremental structural indexing compares detected language as well as content hashes, so adding or removing the first or last C++ source reinterprets unchanged headers. Parser, extraction, and trust-boundary rule upgrades still require `codesage index --full`: the structural index has no interpretation-version stamp. Content hashes remain raw file hashes because watcher, staleness, and feature-map consumers share them; they must not encode parser versions. After deploying an extraction change, rebuild existing indexes explicitly.

Schema migration `0019_file_hash_cache` stores verified content hashes with size, nanosecond modification/change times, and observation time. Incremental discovery reuses a hash only when the stat triple matches and both timestamps precede the observation's whole second; full indexing bypasses reuse. Files still undergo access checks. Platforms without the required timestamps hash normally. This cache does not attest structural parsing or semantic fingerprint freshness.

## MCP tools

- `edit_check` -- compare a complete proposed replacement declaration against a symbol in pinned Git HEAD, without writing source, opening an index, or starting a watcher. Requires absolute `project`, repository-relative `file_path`, exact unqualified `symbol_name`, and `replacement`; optional one-based `line` disambiguates declarations. Reports arity, visibility, and same-scope overload snapshots plus working-file divergence. Provably incompatible caller checks cover same-file Rust free functions reached through explicit `self::` / `super::` paths, without imports, macros, or attributes. Other languages receive syntax-level declaration diffs; methods, indirect/external callers, and unsupported parameter semantics remain unknown. Ambiguous `.h` dialects are refused. This is not a compilation or safety verdict; source and proposed file are capped at 1 MiB, caller output at 100.
- `project_overview` -- one bounded first-call orientation: languages, structural + semantic freshness, feature summary by kind, top-risk files, trust-boundary clusters, per-language test conventions, sample entrypoints, and suggested next calls. Pure aggregation over the index; call once at session start.
- `search` -- semantic search with embedding + reranking; each page carries `confidence` / `margin_pct` / `cliff_at` (relevance-cliff disclosure) and honors opt-in `adaptive_limit`
- `find_symbol` -- symbol definitions by name
- `find_references` -- references to a symbol; each row's `from_symbol` names the enclosing caller (null at file scope). The envelope carries `counts_floor: true` (name-based edges; an empty result means none found, not none exist), `definition_count`, and `ambiguous` with a `note` when several same-named definitions make the rows a union
- `find_similar` -- near-clone detection: functions/methods structurally similar to a named one (MinHash over AST shape, identifiers/literals ignored), ranked by Jaccard. Test files excluded. Needs fingerprints from a reindex.
- `list_dependencies` -- file-level imports/imported-by
- `trace_call_path` -- shortest call chain from one symbol to another, breadth-first over resolved callee edges. Each step names the symbol, its file:line, and `call_line` (the line in the previous step's body where it is invoked). `found: false` carries `note` and `bounded`; `bounded: true` means the search stopped at a limit, so an empty answer is not proof no path exists.
- `from_trace` -- map a pasted stack trace or sanitizer report (Python, PHP incl. Xdebug, Rust, Java, Go, Node/JS, gdb/ASan/UBSan; Kotlin frames parse with the Java syntax, but a Kotlin frame that names a `.kt` file is always `unresolved` since Kotlin sources are not indexed) onto indexed symbols and `file:line`, innermost-first; `frames[0]` is the innermost frame of stack 0 (Java's deepest `Caused by`, with `Suppressed` blocks last; Python's first traceback; ASan's access stack; Go's `[running]` goroutine; one Rust stack per panicking thread), with `stack` per frame and `stacks` + `root_cause_first` on the report. Each frame is `resolved` (`symbol` may be null between definitions; `with_symbol` counts), `ambiguous` (path suffix or qualified name matched several entries, or only a bare name matched -- a lead; `candidates` holds at most 10 (5 when the frame names no file), `candidates_total` the count), or `unresolved` (vendor/stdlib/runtime file outside the index); nothing is guessed, and bare names never cross languages. PHP `#N file(line): func()` frames resolve `func`'s definition by qualified name (`Ns\Class\method` as the index stores it), since that `file:line` is the call site. CLI: `codesage from-trace [FILE|-]`.
- `impact_analysis` -- files affected by changing a symbol or file, with distance and reasons. Opt-in `include_forward` (forward deps), `include_siblings` (same-file symbols), `limit`, and `summary_only` controls; result is an object with `results` plus the requested extras.
- `export_context` -- curated code bundle for a query or symbol, optionally with callers/callees
- `find_coupling` -- files that historically change with a given file (V2b). Each row carries `recurrence` (distinct 90-day calendar windows the pair co-changed in), `span_days` (oldest to newest shared commit), `span_known` (false for rows indexed before recurrence tracking), `recurring` (`span_days >= 30`; `recurrence` alone is informational since a boundary straddle can read 2), `confidence` = P(row file changes | this file changes), and `reverse_confidence`; both confidences are lower bounds (commits touching >30 files add no pair evidence but count in the denominator). Non-recurring pairs rank at half weight (`CODESAGE_COUPLING_RECURRENCE=0` or `false` disables). A page with no recurring pair carries a `note` that says whether the indexed co-change evidence is too short (<30 days), the index needs `git-index --full`, or the coupling is short-burst evidence. Any returned unknown-span row adds a `span unknown` note with the `git-index --full` remedy, including on mixed pages with recurring rows.
- `assess_risk` -- risk score for a single file (V2b slice 1; now blends import-cycle membership alongside churn / fix / blast / coupling / test-gap). `top_coupled` is recurrence-ranked like `find_coupling` (rows carry `span_days` / `span_known` / `recurring`); coupling pressure reads the raw top ten, the coupled-test check reads the raw and ranked pages, and a note names coupled tests that fall below the ranked list. Notes distinguish known one-offs from unknown spans and give the `git-index --full` remedy for unknown spans, including hidden coupled tests and mixed pages.
- `assess_risk_batch` -- per-file risk for N files in one call, no aggregation. Use when you have a list of files and want each one's score; saves the per-file MCP round-trip overhead vs N `assess_risk` calls. For patch-level aggregation use `assess_risk_diff` instead.
- `assess_risk_diff` -- aggregate risk for a patch / set of files (V2b slice 2). Per-file `notes[]` may contain short codes (`"T"`, `"NG"`); resolve via the top-level `_legend` map.
- `recommend_tests` -- tests an agent should run after editing a set of files (V2b slice 2). Three buckets: `primary` (sibling conventions plus any changed file that is itself a test), `coupled` (co-change history; entries carry `span_days` / `recurring` and follow the `find_coupling` ranking, one-offs at half weight; only the top 20 co-change partners per changed file are consulted and a note names inputs where more existed), and `reachable` (test files with a resolved call/import path into the changed set within two hops, ranked by distance then `edge_count`, capped at 50 with `reachable_total` carrying the full count). Changed tests are walked too, so a base test class lists the tests that extend it. Read `reach_walk_capped` before trusting an empty or short list: when it is `true`, `reachable` is a lower bound and `unmodelled` means nothing; `unwalked_files`, `partial_files`, `unindexed_files` (a supported-language path the index does not hold: new file, non-repo-relative path, or excluded by `[index] exclude_patterns`), and `no_symbol_files` say why. `unsupported_files` (no parser: CHANGELOG.md, lockfiles) are skipped and never cap the answer. The walk spends one pool of resolution steps in request order with a floor reserved for each queued input, so put the files you care about first. Fixture directories (`fixtures/`, `stubs/`, `testdata/`, `__snapshots__/`) never appear in any bucket. `CODESAGE_REACH_BUDGET` (default 1,500,000 steps, one pool per request) and `CODESAGE_REACH_DEADLINE_MS` (default 5000; `review_rehearsal` uses 1500 regardless) bound the walk.
- `review_rehearsal` -- predict severity-ranked review objections for a patch (missing tests, high-risk / blast-radius / fix-prone / hotspot files, import cycles, trust-boundary expansion, feature-test gaps, and `scope-spread` when a patch touches ≥4 unrelated feature areas) with hot-symbol evidence. Composes `assess_risk_diff` + `recommend_tests` + drift + feature mapping; read-only, no AI prose. Use as the last step before a commit.
- `session_start` / `session_end` -- snapshot structural state at the start of an editing session, diff at the end. Returns `pass: bool` plus new/resolved cycles, per-file risk regressions on the top-50 baseline, and added/removed files. Note the shape divergence: MCP `session_start` returns the compact `SessionStartReport` (`snapshot_path` + counts), while CLI `codesage session-start --json` prints the full `SessionSnapshot` (file list, cycles, top-risk files).
- `list_features` -- list mapped feature slices, filterable by `kind` (`route`, `cli-command`, `library`, `test-suite`, `service`, `config`, `job`), `language`, or `tag` (0.7.0).
- `find_feature` -- given a file path, return the feature(s) that own it. Routes "what slice owns this file?" without scanning by hand.
- `feature_bundle` -- curated code bundle for one feature slice (entry + owned + tests + context as primary/related chunks, plus the entry symbol's definition and optionally its callers/callees). Same shape as `export_context` but anchored on the feature's pre-curated file list. Returns `found: false` when the `feature_id` is unknown.

Every MCP tool advertises an `outputSchema` (0.7.0); agents that consult it know the result shape before they call. Each schema also declares an optional top-level `_meta` object the server may inject: budget-truncation details (`truncated`, `total_results`, `returned`, `dropped_files`) and staleness annotations (`stale_files`, `stale_warning`).

Responses also include `next`, an evidence-derived `{tool, arguments}` call using retained result rows and an absolute project path. `null` marks empty, unsupported, or terminal evidence; context bundles terminate the chain, and `session_start` waits for edits. Error results preserve their original cause and add a JSON text block with `next: null`. Follow-ups are suggestions, not instructions or evidence that unreturned matches do not exist.

## MCP runtime

`codesage mcp` is the stable client entrypoint. It runs as a stdio shim, starts or connects to the per-user Unix-socket daemon, and forwards MCP JSON-RPC unchanged. The daemon hosts the real MCP server and owns shared project/model/reranker pools across main sessions and subagents.

`codesage mcp --project <abs root>` makes the server default the per-call `project` argument to that root when a `tools/call` omits it. Set automatically by `codesage install` for agents without a CodeSage plugin (Codex, opencode), which otherwise have no way to inject the project path. With no `--project` (the Claude-plugin path) the shim raw-copies stdio with zero overhead.

The daemon co-trusts every process under the same Unix UID. Runtime dirs, socket mode `0o600`, and `SO_PEERCRED` keep other users out, but a compromised same-UID agent can ask the daemon to read any onboarded project index. Use a separate Unix user for untrusted agents that need project isolation. MCP tools are capped for agent safety; CLI commands are the operator surface and intentionally keep broader limits unless a command documents its own cap.

Consistency during reindex: the structural indexer commits in batches of 50 files, so cross-file queries issued while a large reindex is running can see a blend of pre- and post-change state. The per-response `_meta.stale_files` annotation flags results referencing files that changed on disk but aren't reindexed yet. Both windows close when the index pass finishes.

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

`init`, `index`, `overview`, `search`, `brief`, `find-symbol`, `find-references`, `dependencies`, `impact`, `trace`, `from-trace`, `export`, `status`, `mcp`, `daemon`, `watch`, `install-hooks`, `install`, `uninstall`, `cleanup`, `coverage`, `git-index`, `coupling`, `risk`, `risk-batch`, `risk-diff`, `similar`, `tests-for`, `rehearse`, `session-start`, `session-end`, `doctor` (with `--docs`), `map`, `features-list`, `feature-show`, `feature-for`, `feature-bundle`, `trust-boundaries`. (Mirrors the `Commands` enum in `crates/cli/src/main.rs` — audit this list when adding a subcommand.)

`from-trace [FILE|-] [--limit N] [--json]` reads a stack trace or sanitizer report from a file or stdin and prints one line per frame, innermost-first: `#i status path:line symbol`, with ambiguity candidates indented beneath (at most 10, or 5 for a frame with no file, then an `… N more candidates (T total)` line). Same parser and resolution as the `from_trace` MCP tool.

`watch run|status|stop|start [project]` controls the live filesystem watcher. The daemon auto-starts a per-project watcher on the first MCP tool call for that project (reusing the daemon's pooled embedder), debounces edits, and reindexes structural + semantic on change; it self-exits after idle (`CODESAGE_WATCH_IDLE_SECS`) and is reaped on daemon shutdown. Disable per project with `[index] watch = false` or globally with `CODESAGE_WATCH=0`. `watch run` is a foreground instance with its own embedder for debugging; `watch stop` writes a `.codesage/watch.disabled` marker the running watcher honors; `watch start` clears it. The watcher complements the git hooks, it does not replace them: it refreshes structural + semantic content live during a session, but git history intelligence (`git-index`, feeding `assess_risk` / `find_coupling`) and feature mapping still refresh only via the hooks or a full `codesage index`, and the watcher only runs while a daemon is alive.

`install <codex|opencode|all> [--global]` registers CodeSage as an MCP server in agents that have no CodeSage plugin (Codex CLI, opencode), writing their native MCP config (`toml_edit` / `jsonc-parser` CST, comment-preserving and idempotent). It registers the command `codesage mcp --project <abs root>`; `uninstall` removes only CodeSage's entry. Claude Code is not a target — it keeps its `claude mcp add` / plugin registration.

`map` runs the feature mappers (Cargo workspace, composer + Laravel routes, php-src `ext/*`, CMake / autotools, Python `pyproject` / `setup.py` / `__main__`, `package.json` bin + Next.js routes, Go `cmd/*`) and persists features. `codesage index` calls `map` between the structural and semantic passes; `--no-features` skips, and a no-op incremental pass (file-hash state unchanged since the last successful map) skips automatically. `features-list` / `feature-show` / `feature-for` / `feature-bundle` are read-side query commands matching the new MCP tools. `trust-boundaries <file>` is the debugging surface for the per-file boundary tags that feed `assess_risk`.

`cleanup` drops orphaned vec tables from previous model switches, keeping only the active model. Use after benchmarking multiple models. Runs VACUUM automatically.

`doctor --docs [PATH...]` scans markdown for claims that can be checked against the index and working tree and reports the ones that no longer hold: backticked repo-relative file paths and relative link targets that no longer exist, `path:line` anchors past the end of the file or whose nearby backticked symbol has moved, `Owner::member` tokens whose owner is indexed but has no such member, and `UPPER_SNAKE` constants whose stated value (`= 5`, `is 5`, `defaults to 5`, `(default 5)`) differs from the source literal. With no PATH it checks AGENTS.md or CLAUDE.md (following the symlink), README.md, and `docs/**/*.md`, never CHANGELOG.md; a file whose own line is `<!-- codesage-docs: skip-file -->` or that matches `[docs] exclude_patterns` in `.codesage/config.toml` is skipped and named. Explicit PATH arguments bypass `exclude_patterns` (the `git add -f` convention); non-markdown arguments are skipped, and an unreadable directory or non-UTF-8 or unreadable file is named under `files_failed` without aborting the sweep. Fenced code (including blockquote and list containers), indented code (four columns, or four past a list item's content column), HTML comments, globs, URLs, `<placeholder>` tokens, `foo`/`bar`/`baz`/`qux` path segments, `..` segments, env vars, flags, and single words are not claims; a line with an unclosed code span is skipped entirely, links and anchors on it included. Markdown link syntax inside a code span is an example, not a link claim. Deliberately not checked, so they never produce a finding: symbols in foreign code (dependency crates, `std`, unresolvable owners, owners defined only under a fixture or test path, a constant/function/macro homonym owner, bare `foo()` calls, `Type::method`-style placeholders, a camelCase member on a Rust/Python owner, or a method on a type with evidence of unindexed generated or inherited methods), struct fields and enum variants (`Type.field`, `Struct::field`, `Enum::Variant`, neither is indexed; `Type::method()` written as a call is still checked), anchors whose line falls inside no indexed symbol, paths whose parent directory does not exist on disk or in the index (relative links are exempt: a wrong `../` count is exactly the drift they show), extensionless link targets that no `<t>.md` / `<t>/index.md` / `<t>/README.md` / case-insensitive sibling resolves, paths under gitignored directories (`.codesage/`, `target/`), two-segment paths under a ubiquitous root (`docs/`, `scripts/`, `src/`, …) named from a document outside the repo, constants whose stated value carries a unit (`is 30 seconds`), and binary, octal, or exponent-form source literals. From a document outside the repo, symbol claims are not reported at all, and a missing path is reported only with a concrete relocation: a single same-named indexed file (not a basename the index holds twice, nor a generic one like `README.md` or `mod.rs`), or the module having become a directory that holds indexed files of the same kind (`<module>.rs` → `<module>/`). A missing-path finding carries a hint in two cases: when exactly one same-named non-generic, non-fixture file is indexed (`did you mean X` if a directory segment matches, else `a file of the same name exists elsewhere: X`), and when the module became a directory (the hint names the directory); otherwise no hint. `--json` findings carry the hinted paths as `candidates` (empty when there is no hint); a missing explicit PATH is a `files_failed` entry, and an explicit CHANGELOG.md or non-regular file is named under `files_skipped`. Read-only over the index; exit 0 unless `--strict` (exit 1 when anything drifted, a file failed, or no documents were checked; a checked document with zero claims still succeeds); `--json` emits `{ files, files_skipped, files_failed, claims_checked, drifted[] }`.

## Benchmarks

Benchmark harness under `bench/` (curated examples — see `bench/` for the full inventory, including ablation, semble-corpus, concurrency-audit, agent-task, and quality-analysis runners):

- `bench/codesage-bench-runner` — Python runner that executes a YAML corpus of ground-truth cases against `codesage search` and reports miss rate, median first-hit, recall@5, recall@10.
- `bench/extract-eval-cases.py` — mines eval cases from Claude Code session transcripts and git commit history.
- `bench/cleanup-orphan-models.sh` — drops orphaned vec tables from prior model switches.

Corpus YAMLs are not bundled; bring your own. `CODESAGE_BENCH_CORPUS_DIR` (consumed by `/codesage-bench` and `/codesage-eval` plugin commands) points the plugin at the directory holding them.

## Plugin

`plugins/codesage-tools/` ships as a Claude Code plugin: one global `codesage` MCP registration serves every onboarded project, routed by an absolute `project` argument. The registered command remains `codesage mcp`; the shim handles daemon startup and reuse. Slash commands: `/codesage-onboard`, `/codesage-reset`, `/codesage-reindex`, `/codesage-bench`, `/codesage-eval`, `/codesage-prompt-override`, and the four feature-slice review commands (`/codesage-review`, `/codesage-triage`, `/codesage-revalidate`, `/codesage-report`). Marketplace manifest at repo root.

## Git history intelligence (V2b slice 1)

Schema migration `0018_git_author_events` retains per-file author events. After `git-index --full`, risk responses include informational `author_concentration`: author count, dominant share, effective authors (inverse squared-share sum), and `bus_factor` (fewest identities covering at least 50% of weighted commits). Events use a 180-day half-life and a 730-day window. Identity is normalized email, falling back to normalized name; it is not a verified person count, and mailmap changes are not applied retroactively. Legacy or absent author history remains unknown. This signal does not affect the risk score.

`codesage git-index` runs `git log --numstat` and populates `git_files` (per-file churn score with τ=180d decay, fix count, total commits, last commit), `git_co_changes` (file pair weights, min count 3, plus migration `0017`'s `first_observed_at`, `window_mask` — bit `(ts / 90d) % 64` per shared commit, keyed to the unix epoch — and `windows = popcount(window_mask)`), and `git_index_state` (last indexed SHA). Co-change confidence is not stored: `find_coupling` derives P(other | this) as `count / git_files.total_commits` of each side.

Three modes, selected via flags on `codesage git-index`:

- `--full`: fresh rescan. Drops existing rows and walks the entire history. Use after big rebases that rewrite a lot of history, or to rebaseline weekly.
- `--incremental`: scans only `<last_sha>..HEAD` and additively updates counters. Scales pre-existing weights by `exp(-Δt/τ)` so exponential decay stays mathematically exact across runs. Co-change observations below count 3 are retained across passes but hidden from coupling queries until they reach that threshold. Run `codesage git-index --full` after upgrading to recover observations discarded by earlier versions. `window_mask` composes exactly (`mask |= delta`, `first`/`last` take MIN/MAX) because window bits are keyed to a fixed epoch, so `windows` and `span_days` match a full rescan after every incremental pass for rows written by an 0017-aware `--full` (commits inside the 730-day history window; a `--full` still rebaselines rows whose oldest commits have aged out). Rows indexed before 0017 (`first_observed_at IS NULL`) are left unbaselined by incremental passes and keep `span_days` 0 / `recurrence` 1 until `codesage git-index --full`.
- default (no flag, `Auto`): incremental if valid prior state exists and its SHA is an ancestor of HEAD, else full.

`codesage install-hooks` now registers `post-commit`, `post-merge`, `post-checkout`, and `post-rewrite`, each running `codesage git-index --incremental` in the background. Rebased or force-updated history triggers a full rescan automatically (incremental detects when the stored SHA is no longer an ancestor of HEAD and falls back to full).

Two MCP tools consume the tables:

- `find_coupling(project, file_path, limit)` -- top-N files that historically change together with the input, weight-sorted with non-recurring pairs at half weight by default; rows carry `recurrence`, `span_days`, `span_known`, `recurring`, `confidence`, and `reverse_confidence`. CLI: `codesage coupling <file>` prints the measurements and marks one-offs and unknown spans.
- `assess_risk(project, file_path, verbose?)` -- composite risk score (0..1) from churn percentile + fix ratio + depth-2 reverse-dep pressure + coupling pressure + test gap. Returns the score plus human-readable notes for PR descriptions; `verbose: true` (shared with `assess_risk_batch` / `assess_risk_diff`) adds the per-signal decomposition and `top_coupled`. CLI: `codesage risk <file>` (always verbose).

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
