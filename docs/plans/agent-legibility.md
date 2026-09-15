# Plan: agent legibility, one tower of abstractions

Status: design complete, implementation not started. Beads are children of epic `cs-agent-legibility-yhi` in the central `br` ledger; each section below names the bead that owns it by letter, and the table at the end maps letters to ledger IDs. This document does not itself authorize implementation, commit, push, deployment, a release, or external communication.

Written 2026-09-15 from a full read of AGENTS.md and README.md, six read-only code investigations (MCP surface, graph query layer, daemon runtime, storage and features, parser and CLI, plugin guidance), and a first-hand driving session against CodeSage's own index. Every defect claim below cites the file, line, or observed response it rests on. Numbers are counts taken during that session, not benchmarks.

## 1. Stance

CodeSage is a read-only oracle over a codebase. An agent drives it through a loop: orient, locate, understand, assess, edit, verify, commit. CodeSage owns the first four and the planning half of verify. The product question is therefore not "which facts can we compute" but "how few calls, how few tokens, and how little guesswork does an agent spend to reach a correct decision".

Three properties define the target:

- **Intuitive.** An agent that has never read the skill guesses the right tool, the right argument name, and the right reading of the result. Regularity does this, documentation does not.
- **Ergonomic.** The common question costs one call and a small response. Deviations from the happy path (stale, ambiguous, incomplete, capped) are disclosed at the moment they occur, with the remedy attached, and are silent otherwise.
- **Accretive.** Every session leaves the system better informed: which facts were fetched, which were acted on, what the author wrote down as rationale, what a reviewer found. The measurement of whether CodeSage earned its context becomes a product feature, not a transcript-mining exercise.

The measured baseline the design must move: 1.1% CodeSage pick rate over 30 days of sessions and 0 of 10 on the controlled harness (README, `notes/20260804-serve-dont-recommend.md`); 1,215 brief-hook fires yielding 5 served payloads and 1 acted (`bench/brief-efficacy/RESULTS.md`). Recommendation-shaped surfaces have not moved these numbers. This plan changes the shape of the surface itself.

## 2. What the driving session showed

Observed on this repository at 0.31.0 with a fresh index. Each row is an evidence anchor for a bead.

| # | Observation | Cause (verified) | Bead |
|---|---|---|---|
| 1 | `assess_risk crates/graph/src/search.rs` reports `top_symbols[0]` = `path`, "hot: 3 lines, 1884 refs". `search.rs:4715` is `fn path(p: &str)` inside a `#[cfg(test)]` module. The index holds 1,886 call refs named `path` across 94 files and 3 definitions. | `compute_top_symbols` (`git_history/risk.rs:1008`) counts references by name without the import-aware reverse resolution `impact.rs:846` applies, and without excluding test-module symbols. | C1 |
| 2 | `list_dependencies crates/graph/src/search.rs` returns roughly 80 `imports`, most of them `super::<item>` from the file's own test modules (23 `use super::` lines across 10 `#[cfg(test)]` modules), plus unresolved `std::` items. `imported_by` has 2 rows. | Raw import refs are returned unfiltered and unresolved (`lookups.rs:100`). | C2 |
| 3 | `project_overview.entrypoints` lists 15 `bench/*.py` `__main__` modules. The `codesage` binary, the MCP server, and the daemon do not appear. | `overview.rs:69-88` caps at 15 by kind with no product ranking; the Python `__main__` mapper floods the sample. | B2 |
| 4 | `project_overview.top_risk_files`: three of the top four are test files (`crates/cli/tests/mcp_daemon.rs` 0.73, `bench/test_bench_fixes.py` 0.72, `plugins/codesage-tools/tests/test_review_state.py` 0.69). | The risk ranking has no test segregation; `is_test` is a discovery-time path heuristic that never reaches a row. | C3, B2 |
| 5 | `project_overview.freshness` said "index is 5 commits behind HEAD"; `codesage index` then re-parsed 0 structural and 0 semantic files. The five commits touched only unindexed paths (Cargo.lock, CHANGELOG, workflows). | `drift.rs:133` compares stored SHA to HEAD and counts commits; it never asks whether any indexed file changed. | D1 |
| 6 | `.codesage/hooks.log`: a post-merge hook run started 2026-09-14 07:09:37, never logged an exit, and left `hook-index.lock/` in place for 24 hours. Two later hook fires logged "skip: another index already running". | The lock is reaped only by the next fire after 30 minutes (`post-commit:97-103`); nothing else observes hook health. | D3 |
| 7 | `recommend_tests [search.rs]`: `primary` = all 21 integration tests of the crate, `reachable` = 0, `unmodelled` = 45, no inline `#[cfg(test)]` tests, no runnable command. | Rust convention resolves to `<crate>/tests/*.rs`; inline tests are a documented gap; output is paths, not commands. | C4 |
| 8 | `search` default page for a natural-language query: `confidence: high`, `cliff_at: 2`, yet 5 full snippets returned; rows 3 to 5 were bench and test code. | `adaptive_limit` is opt-in; below-cliff rows carry full `content`. | C5 |
| 9 | `find_symbol render`: 4 rows including the `mod render;` declaration; no `ambiguous` flag, no `definition_count`, no handle to pass onward; `next` picked one row arbitrarily. | `find_symbol` has no ambiguity envelope (`lookups.rs:12`); symbols carry no identifier (`impact.rs:838`). | A1, A2 |
| 10 | `impact_analysis target=search` fails with "ambiguous symbol"; `find_references search` would have returned the union with `ambiguous: true`. | Two ambiguity policies: `impact.rs:324-341` bails, `lookups.rs:17-82` unions. | A1 |
| 11 | Error on a non-existent `project` path carries no tool name and no code; a parameter-validation failure bypasses error normalization entirely (`dispatch.rs:584-590`). | Two error layers that disagree (`render.rs:286-329`, `dispatch.rs:203-224`). | A4 |
| 12 | Every response carries exactly one `next`, and for 14 of 22 tools it is `list_dependencies` on the first path in the result (`next.rs:124-157`). | The follow-up whitelist has 4 tools; the default is fixed. | B3 |

Two facts from the investigations shape everything below:

- **`outputSchema` never reaches the model.** Claude Code forwards `name`, `description`, and `inputSchema` only (`~/ai/wiki/tools/claude-code-quirks.md`). Field semantics written into output schemas are invisible to the agent. The only channels that reach the model are the description, the input schema, the skill, the hint, and the response body itself.
- **Descriptions are the largest always-on guidance surface**: about 24,300 characters across 22 tools, roughly 6,200 tokens per session, versus 1,100 for the skill and 2,000 for the generated hint. They are also where most caveats live, so an agent pays for every caveat on every session whether or not it applies.

## 3. The tower

Each layer has one contract. Higher layers are compositions of lower ones and expose nothing the lower layer cannot explain.

| Layer | Contract | Today | Target |
|---|---|---|---|
| L0 Substrate | Parsers, embeddings, SQLite, daemon pools. Invisible except through cost and freshness. | Solid. | Unchanged except freshness reporting (D1) and overlay (D2). |
| L1 Identity | Every entity has one legible, hand-constructible handle. Every tool accepts a handle or a string in one `target` field and resolves through one resolver with one ambiguity policy. | Files by path, features by `feat_` id. Symbols and chunks have no identity; seven input spellings. | Handles on every row; one resolver; one policy. (A1, A2) |
| L2 Envelope | One response shape. Silence means exactness; deviations (stale, ambiguous, incomplete, capped) are disclosed with a remedy. Errors carry a code and a remedy. | Thirteen incompleteness vocabularies; `_meta` allowlists; two error layers. | One envelope, one error contract, one detail knob. (A3, A4, A5) |
| L3 Cards | One call returns everything the index knows about one entity, compact, with handles that expand into L4 questions. | Five to seven calls per entity. | `describe` for file, symbol, feature, directory. (B1) |
| L4 Relations | Questions between entities: references, dependencies, impact, paths, coupling, clones. | Present; irregular envelopes; test noise. | Same tools on L1 and L2 contracts; test-aware. (C1 to C5) |
| L5 Judgments | Risk, tests, rehearsal, session diff, edit check. Composed from L3 and L4. | Present and well disclosed; outputs are facts, not actions. | Actionable outputs: commands, remedies. (C4, C7) |
| L6 Orientation | Product-first entry, module map, ranked next steps. | Bench scripts and test files dominate. | `project_overview` v2; `next[]` derived from data. (B2, B3) |
| L7 Guidance | The agent learns semantics at the moment they apply, from the response, or on demand. Always-on text is minimal and consistent. | 9,400 always-on tokens across three disagreeing surfaces; four tools taught nowhere. | Descriptions at 60 words; one routing document; `help` on demand. (E1 to E3) |
| L8 Freshness | Every answer states what it describes: which index generation, whether the working tree differs, whether the answer covers dirty files. | Best-effort `_meta.stale_files`; SHA-count drift; 30 s watcher debounce; hooks can die silently. | Per-call generation header; content-based drift; working-tree overlay; hook health. (D1 to D3) |
| L9 Composition | Several questions in one round trip under one budget. | None. | `batch`. (B4) |
| L10 Accretion | Sessions record what was fetched and what was acted on; authored rationale and reviewer findings attach to entities. | Session snapshots; findings ledger in the plugin; rationale for Rust and Python. | Session ledger with acted-on correlation; findings in cards; rationale for all languages; persisted signatures. (F1 to F4) |

## 4. Contracts

### 4.1 Target grammar and handles (A1, A2)

One field, `target`, on every tool that names an entity. It accepts:

```
target  := handle | path | path ":" line | qualified | name | feature_id | route | command | text
handle  := "sym:" path "#" qualified                  a symbol, line-independent
         | "sym:" path "#" qualified "@" line_start   one overload when several share a name in one file
         | "file:" path
         | "dir:" path
         | "chunk:" path ":" start "-" end
         | "feat_" hex16                              unchanged
route   := "route:" METHOD " " path                   e.g. route:POST /api/login
command := "cmd:" name                                a mapped CLI command
```

Handles are chosen for legibility over opacity: an agent can write `sym:crates/graph/src/search.rs#search_page` after a `Read` without a lookup, the handle survives reindexing and edits that do not rename or move, and a rename fails cleanly (not found, with the resolver's nearest candidates) instead of silently pointing elsewhere. A content-hash id would be stable under moves but opaque, and opacity costs a call each time it is met. `feature_id` keeps its current derivation.

Every row any tool emits carries `handle`. `Symbol` rows gain `handle`; `Reference` rows gain `from` (handle of the enclosing symbol, null at file scope) and `to` (handle when resolved, null when external); search rows gain a `chunk:` handle; features already carry `feat_`.

One resolver (`resolve_target`, in `crates/graph`) replaces the per-tool string matching in `lookups.rs`, `impact.rs`, `bundle.rs`, `call_path.rs`, and `edit_check.rs`. It returns:

```json
{"input": "search", "kind": "symbol|file|dir|chunk|feature|route|command|text",
 "resolved": [{"handle": "sym:crates/graph/src/search.rs#search", "kind": "function", "line_start": 617, "line_end": 628, "is_test": false, "confidence": 1.0, "via": "exact|qualified|unique|import|casefold|suffix"}],
 "ambiguous": false, "candidates_total": 1}
```

One ambiguity policy replaces the two that exist. Tools whose answer is a union of per-definition answers (`find_references`, `find_similar`, `search` with a mention) return the union with `target.ambiguous: true` and per-row `to`/`from` handles so the agent can split it. Tools whose answer would be wrong as a union (`impact_analysis`, `trace_call_path`, `describe`, `edit_check`, `bundle`) return no data, `target.ambiguous: true`, `candidates[]` with handles, and the error code `E_AMBIGUOUS` with `remedy.arguments.target` set to the first candidate. Neither path bails with prose.

Existing argument names (`name`, `symbol_name`, `file_path`, `from`, `to`, `feature_id`) remain accepted for one minor release as aliases of `target`, and `find_references` keeps `name` as its primary spelling since the target there is a name by nature. `is_file` and `is_symbol` become unnecessary and are removed after the alias window; the grammar disambiguates.

Overloads: several definitions with one qualified name in one file (C++ overloads, Rust methods of the same name on different `impl` blocks, Go methods on different receivers) resolve to `ambiguous: true` with `@line` handles as candidates. The resolver reports `overloads: n` so the agent knows the ambiguity is intra-file.

### 4.2 Response envelope (A3)

Every tool response is:

```json
{
 "tool": "find_references",
 "index": {"generation": 1312, "head": "9c6a2be", "structural": "fresh"},
 "target": {...},            
 "data": {...},
 "completeness": {"kind": "exact"},
 "cost": {"ms": 11, "bytes": 3820, "detail": "compact"},
 "next": []
}
```

with the rule **silence means the good case**. `index.structural` is omitted when `fresh`; `index.dirty_paths` appears only when a path in `data` differs from the indexed content; `target` is omitted when the input resolved to exactly one entity by exact match; `completeness` is omitted when `exact`; `next` is omitted when empty. A happy-path response therefore adds about 60 bytes over today's payload, and any deviation is the only unusual thing in the response.

`completeness.kind` is one enum replacing today's vocabulary:

| kind | Replaces | Meaning | `recover` carries |
|---|---|---|---|
| `exact` | (absence) | Every row that exists is present. | nothing |
| `floor` | `counts_floor`, `unmodelled` | Name-based edges; rows are a lower bound, absence is not proof. | the reason class (dynamic dispatch, macros, unresolved imports) |
| `bounded` | `bounded`, `reach_walk_capped`, `frontier_capped`, `callers_truncated` | A search stopped at a depth, step, or time limit. | `{tool, arguments}` for a narrower or deeper retry, or the CLI command when MCP is capped |
| `truncated` | `_meta.truncated`, `truncated`, `reachable_capped` | Rows were computed and dropped for budget. | `total`, `returned`, `{arguments}` with `offset`/narrower scope |
| `clamped` | `_meta.clamps` | A requested parameter exceeded its cap. | `requested`, `applied` |
| `unscored` | `unscored` | A term was never measured (no git row, no semantic rows). | the command that would measure it |
| `partial` | `unwalked_files`, `partial_files`, `unindexed_files`, `no_symbol_files`, `unsupported_files`, `_meta.coverage` | Some inputs were not covered. | per-input reason |

Several kinds may apply; `completeness.kinds[]` lists them all and `kind` is the most severe. `confidence` on `search` keeps its name (compatibility, see README) and `find_coupling`'s `confidence` is renamed `p_cochange` with `p_reverse`; the name collision is a documented source of misreading.

`index.generation` is the daemon's DB commit generation already used by the overview cache (`overview_cache.rs:87`), so two responses with equal generation describe the same index state. `index.head` is the short indexed SHA. `index.structural` is `fresh`, `behind` (indexed content differs from HEAD for N indexed files, see D1), or `dirty` (working tree differs from the index for a path in this response), and `index.semantic` is `fresh`, `partial`, or `none`.

`_meta` is retired. Its keys map onto the envelope: `truncated`, `clamps`, `coverage` into `completeness`; `stale_files`, `stale_warning` into `index.dirty_paths`; `test_override`, `ranking_recomputed` into `cost.notes[]`. The `PATH_KEYS` allowlist (`render.rs:337-357`) is replaced by handle-carrying rows: every row that names a file carries `file:` or `sym:` handles, and staleness is computed from handles, so the allowlist cannot drift.

Compatibility: one minor release emits both the legacy fields and the envelope, gated off with `CODESAGE_ENVELOPE=legacy`; the next removes the legacy fields. Pre-1.0 minor bumps may break, and the plugin's review-state helper and agent allowlists are updated in the same commit.

### 4.3 Error contract (A4)

Every failure, including parameter validation and project routing, returns:

```json
{"tool": "impact_analysis", "error": {"code": "E_AMBIGUOUS", "message": "3 definitions named `search`",
  "remedy": {"tool": "impact_analysis", "arguments": {"target": "sym:crates/cli/src/mcp/mod.rs#Server::search"}}},
 "work_continuing": false, "request_id": 38}
```

Codes: `E_PARAM`, `E_PROJECT_PATH` (relative or missing), `E_NOT_ONBOARDED`, `E_SCHEMA_TOO_NEW`, `E_NOT_FOUND`, `E_AMBIGUOUS`, `E_EMPTY_INPUT`, `E_OVER_CAP`, `E_MODEL` (download or load), `E_DB_BUSY`, `E_SATURATED`, `E_TIMEOUT`, `E_CANCELLED`, `E_SHUTDOWN`, `E_INCOMPLETE`, `E_INTERNAL`. `remedy` is a tool call or a shell command, never prose alone: `E_NOT_ONBOARDED` carries `{"command": "codesage init && codesage index"}`, `E_SCHEMA_TOO_NEW` carries the upgrade command, `E_MODEL` carries `codesage doctor`. `dispatch.rs:584-590` and the routing preflight (`dispatch.rs:601-635`) route through the same constructor so no path emits bare text.

### 4.4 Detail and budget (A5)

One knob, `detail: "compact" | "standard" | "full"`, on every tool that returns rows, and one optional `budget_tokens` the server may lower to its cap. `compact` returns handles plus one line per row (path, line, kind, one-line reason) and no `content`; `standard` is today's shape; `full` adds snippets, per-signal decompositions, and `top_coupled`. `verbose` and `summary_only` become aliases of `full` and `compact` for one release. Default is `compact` for `find_references`, `impact_analysis`, `list_dependencies`, `features`, `describe`, and `standard` for `search` and bundles. `cost.bytes` and `cost.ms` are reported on every response so an agent can learn the price list by using the system, which no description can teach.

Related record: `cs-sweep-signature-collapse-w2q` (payload ballooning, observation-gated). The detail knob is the general mechanism; that bead's specific collapse rule stays gated.

### 4.5 Test-awareness as an attribute (C3)

`is_test` is computed at discovery (`TEST_LIKE_EXCLUDE_PATTERNS`, `discover.rs:411-438`) and today influences only search demotion and coupling exclusion. It becomes a stored attribute on `files` (migration, additive) and a serialized attribute on every file, symbol, and reference row, with a symbol inside a `#[cfg(test)]` module, a `tests` module, or a test-decorated function marked `is_test: true` by the parser (Rust `#[cfg(test)]` and `#[test]`, Python `test_` and `pytest` markers, Go `_test.go`, PHP `*Test.php`, JS `describe`/`it` files). Consumers: `top_symbols` and top-risk rankings exclude tests by default (`include_tests: true` to widen), `list_dependencies` reports test-module imports under `test_imports`, `find_references` rows carry `is_test` so a caller count can be read as product callers, `search` demotion uses the attribute instead of re-matching globs per query.

## 5. New capabilities

### 5.1 `describe` (B1)

One call, one entity, everything the index knows, compact, with handles that expand into L4 questions. It also serves as the resolver: an ambiguous `target` returns candidates and nothing else.

File card:

```json
{"handle": "file:crates/graph/src/search.rs", "language": "rust", "lines": 5307, "is_test": false,
 "indexed": {"generation": 1312, "dirty": false},
 "symbols": {"total": 212, "product": 61, "test": 151,
             "top": [{"handle": "sym:...#search_page", "kind": "function", "fan_in": 44}, ...]},
 "imports": {"internal": 11, "external": 19, "test_only": 23, "top_internal": ["file:crates/storage/src/lib.rs", ...]},
 "imported_by": {"total": 2, "top": ["file:crates/graph/src/bundle.rs", "file:crates/graph/src/lib.rs"]},
 "features": ["feat_be0460a7b60c45a7"],
 "risk": {"score": 0.58, "notes": ["hotspot: churn percentile 99%", "fix-heavy: 8/18 commits"]},
 "coupling": [{"file": "file:...", "p_cochange": 0.6, "recurring": true}],
 "tests": {"primary": ["file:crates/graph/tests/..."], "command": "cargo test -p codesage-graph"},
 "trust_boundaries": ["process-exec", "secrets", "serialization", "concurrency"],
 "findings": {"open": 0},
 "expand": {"callers_of_top": {"tool": "find_references", "arguments": {...}}, "impact": {...}, "full_risk": {...}}}
```

Symbol card: handle, kind, signature (once F4 lands), lines, `is_test`, `rationale[]`, reference counts by kind split product/test, top callers and callees as handles, clones above 0.85 with Jaccard, owning feature, hotness rank within its file, cycle membership, `expand` for `find_references`, `impact_analysis`, `trace_call_path`, `find_similar`, `bundle`.

Feature card: the `FeatureRecord` plus max and mean risk over owned files, the test command, boundaries, and `expand` to `bundle`.

Directory card: the module map for one subtree: files, lines, symbols, languages, test share, top fan-in files, features rooted there, risk summary. This is the artifact the driving session needed first and could not get.

Cost discipline: `describe` runs only the cheap facts by default. `risk` is included because it is the fact agents act on, but it reuses the shared `WalkCache` and the overview's cached ranking, and `sections: [...]` lets a caller drop it. `cost.ms` on the response tells the agent what a card costs on this project. The serve-don't-recommend memo's measurement that `assess_risk` can cost 464 to 675 ms on a large PHP index applies here: a `describe` on such a project must still return within the interactive class deadline, so `risk` is computed under a per-section deadline and reported as `unscored` with `recover` when it is cut.

### 5.2 `batch` (B4)

```json
{"calls": [{"id": "a", "tool": "describe", "arguments": {...}}, {"id": "b", "tool": "find_references", "arguments": {...}}],
 "budget_tokens": 6000}
```

Up to 8 calls, executed in order under one admission lease, one `WalkCache`, one deadline, and one response budget; per-call envelopes are returned under `results.<id>`, and a failed call does not fail the batch. The daemon already shares pools across sessions; `batch` shares them across the questions one agent asks at once and removes N minus one round trips and N minus one envelopes. Composition is deliberately flat: no call may reference another's result. Chaining stays in the agent.

### 5.3 `help` (E3)

Because output schemas never reach the model, field semantics need an on-demand channel. `help {tool?, field?, code?}` returns the semantics of a tool, a field, or an error code, the recovery ladder for an empty or incomplete result, and the current price list from the daemon's per-tool latency histograms. It is the place the 6,200 tokens of caveats move to. It is also the mechanism by which a new tool can ship without growing the always-on budget.

### 5.4 Search explain (C7)

`search {explain: true}` attaches `trace[]` per row: `(stage, before, after, reason)` for dense, BM25 fusion, symbol boost, definition boost, rerank blend, path penalty, saturation, mention anchor. RRF fusion currently launders its score back into `distance` (`search.rs:446`) and must carry dense rank, BM25 rank, and fused score forward on `RawSearchRow` instead. This is a maintainer instrument first (diagnosing misses on the semble corpus without ad hoc scripts), and an agent instrument second (an agent that sees a target at rank 4 because of directory saturation can rephrase). It is the TODO.md "Search Explainability" item; `debug_search_miss` is out of scope until `explain` exists.

## 6. Orientation v2 (B2, B3)

`project_overview` becomes the project card:

- `module_map`: top-level directories, crates, or packages with file count, lines, symbols, languages, test share. Ranked by product symbols.
- `entrypoints`: ranked by a product score: kind weight (service, route, cli-command above library above test-suite), path class (product above `bench/`, `scripts/`, `examples/`), fan-in of the entry file, churn. Kinds are diversified within the cap of 15 so one mapper cannot flood the sample.
- `top_risk_files`: product files only by default; `top_risk_tests` separately; `include_tests` restores the merged list.
- `hot_symbols`: top 10 product symbols by fan-in, as handles.
- `freshness`: `commits_behind` stays, joined by `indexed_files_behind` (indexed files whose HEAD blob differs from the indexed content) so "5 behind, 0 files affected" reads correctly (D1). `hook_health` reports the last hook run, its exit, and whether a lock is held by a live or dead process (D3).
- `suggested_next_calls` is removed. It is a static routing table repeated in the skill, and the memo's measurement says agents do not act on it. `next[]` replaces it with data-derived suggestions only: a stale index carries the reindex command; a project with no git history carries `codesage git-index`; otherwise the list is empty.

`next[]` on every tool becomes a ranked list of at most three, derived from the data and keyed by the question the response leaves open: an ambiguous target lists the candidates as calls; a floor result on `find_references` for a widely used name suggests `impact_analysis` on the resolved handle; a `describe` file card suggests the top symbol's card; `search` with a high cliff suggests `describe` on the top file. `list_dependencies` stops being the default. Each entry carries `why` in ten words or fewer. The whitelist in `next.rs:8-38` is replaced by the resolver's handle types.

## 7. Guidance v2 (E1, E2, E3, E4)

- **Descriptions** (E1): at most 60 words each: the question the tool answers, when not to use it, one contrast with the nearest tool. No field semantics, no caveats, no bold "prefer this over Grep" prose; the measured effect of that prose on pick rate was zero. Target total under 1,500 tokens for 22 tools, measured from `tools/list` input schemas plus descriptions, before and after.
- **One routing document** (E2): the `codesage-retrieval` skill becomes the only routing text. It covers every advertised tool including the four taught nowhere today (`trace_call_path`, `from_trace`, `edit_check`, `find_similar`), the recovery ladder (empty, floor, bounded, truncated, stale: what each means and the next call), the freshness rule (history-derived facts survive edits; structural facts do not), the ordering rule for `recommend_tests` inputs, and a short price list. The generated `.claude/CLAUDE.md` hint shrinks to about ten lines: the project path, "load the skill", the patch file-set rule, and the review-command block. The hint version stamp (`codesage-onboard:212`, stuck at 0.13.0) tracks the plugin version. The prompt-override fragment is regenerated from the skill or retired; three surfaces that disagree are worse than one.
- **`help`** (E3): see 5.3.
- **Parity** (E4): `codesage trust-boundaries`, `feature-show`, `brief`, and `map` exist on the CLI without MCP equivalents; `edit_check` exists on MCP without a CLI. `describe` subsumes `trust-boundaries` and `feature-show`; `brief` stays CLI (hook payload); `edit_check` gains `codesage edit-check`. `daemon_stats` is either advertised or reachable through `help {tool: "daemon"}`; a tool that exists but cannot be discovered is a trap.

## 8. Freshness and overlay (D1, D2, D3, D4)

- **D1 Content-based drift and per-call header.** `check_drift` gains `indexed_files_behind`: for the indexed set, compare stored `content_hash` against the HEAD blob of each path. Cheap path: `git diff --name-only <indexed_sha>..HEAD` intersected with indexed paths, hashed only for the intersection. Every response carries `index.{generation, head, structural}` from the envelope. `edit_check` receives the same header; today it is the one tool without staleness annotation (`mod.rs:385`). This is the concrete case `cs-ma4` asked for: an annotation that was wrong in direction (stale reported, nothing stale) rather than missing.
- **D2 Working-tree overlay.** At call time the daemon lists indexed files whose working-tree hash differs from the indexed hash (the `file_hash_cache` stat triple makes the check a stat per file), parses each into an in-memory structural overlay (symbols and refs, no embeddings), and serves overlay facts for `describe`, `find_symbol`, `find_references`, `impact_analysis`, `trace_call_path`, and `edit_check`. Bounds: at most 50 dirty files, 2 MiB total, 500 ms; beyond that the call falls back to indexed facts with `index.structural: dirty` and `dirty_paths`. Overlay entries are cached by `(path, hash)` and evicted with the watcher. The watcher's 30 s debounce stops being a correctness problem for structural answers; it remains the mechanism for semantic refresh. This is the TODO.md "Working-Tree Shadow Index" item.
- **D3 Hook health.** Hooks log `start pid=N` and always log an exit line from the EXIT trap; the daemon and `codesage doctor` report a `hook-index.lock` older than the run interval whose recorded pid is dead, and reap it at daemon start and on `project_overview`. `hook_health` appears in the project card.
- **D4 Watcher start.** Evaluate starting the watcher on the first index-backed call of any kind rather than the first semantic query (`state.rs:69-75`), and refreshing feature mapping on debounce when the changed set touches a mapper's inputs. Decision-gated on measured cost; the overlay (D2) covers the correctness half.

## 9. Accretion (F1 to F4)

- **F1 Session ledger.** When a `session_id` is present (or per MCP connection when not), the daemon appends `{ts, tool, target_handles[], detail, bytes}` to `.codesage/sessions/<id>.calls.jsonl`, handles only, no query text, matching the diagnostics retention policy. `session_end` correlates: files edited during the session (snapshot vs working tree and `git diff`) against files whose cards, risk, tests, or references were fetched, and reports `acted_on: {fetched_then_edited, edited_unfetched, fetched_unedited}` plus which recommended tests appear in the edited set. This is the in-product instrument the serve-don't-recommend memo asked for, with a discrete denominator per session and no transcript replay. It does not establish causality; it establishes whether CodeSage's answers were in the path of the work, per tool, per project, over time.
- **F2 Findings in cards.** `describe` on a file or feature includes `findings: {open, ids[]}` from `.codesage/findings/`. The memo deferred serving findings unasked; a card is asked for.
- **F3 Rationale for all languages.** `rationale.rs` covers Rust and Python. Comment extraction through tree-sitter comment nodes is language-generic; extend to the remaining seven and bump `STRUCTURAL_INTERPRETATION` `extraction`.
- **F4 Persisted signatures.** The parser computes signatures only transiently inside `edit_check` (`edit_check.rs:225-279`). Persist `signature`, `arity`, `visibility`, `params` (JSON), `returns` on `symbols` (additive migration, `extraction=2`). Cards and `find_symbol` show them; `edit_check` compares against stored HEAD signatures without re-parsing; "public API surface of this file" becomes a card section. Schema migration and public output change: human gate before implementation.

## 10. Surface map

Principle: one noun per tool, verbs as arguments, handles everywhere. Aliases for one minor release; the plugin's agent allowlists and the review-state helper are updated in the same commit.

| Today | Target | Note |
|---|---|---|
| `find_symbol` | `find_symbol {target}` | gains `ambiguous`, `definition_count`, handles |
| `find_references` | `find_references {target}` | rows gain `from`, `to`, `is_test` |
| `impact_analysis` | `impact_analysis {target}` | `is_file` removed; ambiguity via `E_AMBIGUOUS` |
| `trace_call_path` | unchanged shape, envelope | |
| `find_similar` | `find_similar {target}` | |
| `list_dependencies` | `list_dependencies {target}` | resolved to files; externals collapsed; `test_imports` |
| `search` | `search {query, explain?}` | compact below the cliff by default |
| `list_features`, `find_feature` | `features {kind?, language?, tag?, since?, target?}` | one noun |
| `export_context`, `feature_bundle` | `bundle {target}` | target is `feat_`, `sym:`, `file:`, or text |
| `assess_risk`, `assess_risk_batch`, `assess_risk_diff` | `assess_risk {targets[], aggregate?}` | one path is a batch of one; `aggregate: true` yields today's diff rollups |
| `find_coupling` | unchanged shape, `p_cochange` | |
| `recommend_tests` | `recommend_tests {targets[]}` | gains `commands[]` |
| `review_rehearsal` | unchanged shape, `remedy` per objection | |
| `session_start`, `session_end` | `session {action, session_id}` | ledger under F1 |
| `edit_check` | unchanged, `codesage edit-check` CLI | |
| `from_trace` | unchanged shape, envelope | |
| `project_overview` | project card | 6 |
| new | `describe`, `batch`, `help` | |

Twenty-two tools become nineteen. The count is a consequence, not the goal; the goal is that an agent who has learned one tool has learned them all.

## 11. Sequencing

Phases are ordered by dependency, not by value; the highest-value items (cards, orientation, guidance) sit on the contracts and cannot ship cleanly before them.

| Phase | Beads | Depends on | Ships as |
|---|---|---|---|
| 0 Contracts | A2 handles, A1 resolver and ambiguity policy, A3 envelope, A4 errors, C3 `is_test` attribute | nothing | one minor release, legacy fields retained |
| 1 Defects on the contracts | C1 hotness collision, C2 dependencies noise, C5 search compactness, D1 content drift and header, D3 hook health | A2, A3, C3 | same or next release |
| 2 Cards and orientation | B1 `describe`, B2 overview v2, B3 `next[]`, A5 detail knob | phase 0 | next minor |
| 3 Composition and guidance | B4 `batch`, E1 description diet, E2 routing document, E3 `help`, E4 parity, C4 test commands | B1, A3 | next minor; legacy fields removed |
| 4 Freshness | D2 overlay, D4 watcher policy | D1, A3 | gated on measured cost |
| 5 Accretion | F1 ledger, F2 findings in cards, F3 rationale, F4 signatures, C7 explain | B1, A2 | gated per bead |
| 6 Surface consolidation | the merges in section 10 | phases 0 to 3 | one minor with aliases, one without |

Human gate (AGENTS.md): A3, A4, and section 10 change the public MCP API; C3 and F4 add schema migrations. Those beads are created `state:needs-human` and require approval before implementation. Everything else is additive.

## 12. Measurement

The plan is complete when these numbers move, measured with the harnesses that already exist in `bench/`:

| Measure | Baseline | Target | Instrument |
|---|---|---|---|
| Always-on guidance tokens (descriptions + input schemas + skill + hint) | about 9,400 | under 4,000 | `tools/list` token count plus word counts |
| Median CodeSage calls to answer a fixed orientation question set on this repo | to measure before phase 2 | halved | `bench/agent-task` question set, before and after |
| Median response bytes at default detail | to measure before phase 0 | halved | `daemon stats` payload gauge (to add: bytes per tool) |
| False stale warnings | 1 of 1 observed | 0 | D1 regression test |
| Test symbols in `top_symbols` for product files | 5 of 5 on `search.rs` | 0 by default | C1 regression test |
| Pick rate on the controlled harness | 0 of 10 (2026-04-30) | measured, reported | `bench/agent-tool-selection-harness.py` |
| Acted-on rate per tool | unmeasurable | reported per session | F1 ledger via `session_end` |

Pick rate is listed as measured, not targeted. The hypothesis is that regularity, smaller descriptions, and served disclosure raise it; the harness decides.

## 13. Non-goals and risks

- No write or edit tools. `describe` and `batch` are reads. F1 writes the ledger under `.codesage/`, which the plugin already writes.
- No LLM at index time. Cards, resolver, and overlay are deterministic.
- No blocking hooks. Guidance changes shape; enforcement stays rejected (TODO.md non-goals).
- No cross-repository queries. `batch` is single-project by construction; `docs/decisions/retrieval-roadmap.md` owns federation.
- Risk: the envelope migration touches every tool and every consumer. Mitigation: one release with both shapes, the plugin updated in lockstep, `CODESAGE_ENVELOPE=legacy` as the escape hatch, and the `mcp_daemon` inventory test extended to assert the envelope on every advertised tool.
- Risk: `describe` becomes expensive on large indexes. Mitigation: per-section deadlines, `sections`, cached ranking, `cost.ms` disclosure, and a hard interactive-class deadline; the serve-don't-recommend memo's 464 to 675 ms `assess_risk` measurement is the reference case.
- Risk: handles by path and qualified name collide for overloads. Mitigation: `@line` disambiguator and `overloads: n` disclosure; the natural key `uq_symbols_identity` already exists in storage.
- Risk: the description diet lowers pick rate further before `help` and the skill catch up. Mitigation: E1, E2, and E3 ship in one release.

## 14. Bead map

Epic `cs-agent-legibility-yhi`. Children are `cs-agent-legibility-yhi.N`; `br show <id>` gives the evidence, acceptance criteria, and blocking dependencies. Beads marked gate carry `state:needs-human`.

| Letter | ID suffix | Title | P | Gate |
|---|---|---|---|---|
| A2 | .1 | Legible handles on every emitted row | 1 | |
| A1 | .2 | One target grammar, one resolver, one ambiguity policy | 1 | |
| A3 | .3 | Unified response envelope | 1 | gate |
| A4 | .4 | Structured error codes with remedy | 1 | gate |
| C3 | .5 | `is_test` as a stored, serialized attribute | 1 | gate |
| C1 | .6 | `top_symbols` name-collision hotness | 1 | |
| C2 | .7 | `list_dependencies` noise | 2 | |
| C5 | .8 | `search` below-cliff content | 2 | |
| D1 | .9 | Content-based drift and per-call index header | 1 | |
| D3 | .10 | Hook health and dead-pid lock reaping | 2 | |
| B1 | .11 | `describe` cards | 1 | |
| B2 | .12 | `project_overview` v2 | 1 | |
| B3 | .13 | Ranked `next[]` | 2 | |
| A5 | .14 | `detail` knob and `budget_tokens` | 2 | |
| B4 | .15 | `batch` | 2 | |
| E3 | .16 | `help` | 2 | |
| E1 | .17 | Description diet | 1 | |
| E2 | .18 | Skill as the single routing document | 2 | |
| E4 | .19 | CLI and MCP parity | 3 | |
| C4 | .20 | `recommend_tests` runnable commands | 2 | |
| D2 | .21 | Working-tree structural overlay | 1 | |
| D4 | .22 | Watcher start and feature refresh (evaluate) | 3 | |
| F1 | .23 | Session ledger and acted-on correlation | 2 | |
| F2 | .24 | Findings in cards | 3 | |
| F3 | .25 | Rationale for all languages | 3 | |
| F4 | .26 | Persisted signatures | 2 | gate |
| C7 | .27 | `search` explain | 2 | |
| G1 | .28 | Surface consolidation | 2 | gate |
| H3 | .29 | Daemon project-state map growth | 3 | |
| H5 | .30 | Parser reference coverage gaps | 3 | |
| H4 | .31 | `feature_id` doc comment | 4 | |

## 15. Relationship to existing records

TODO.md Priority 1 "Universal Target Resolver" is A1 with A2; Priority 1 "Working-Tree Shadow Index" is D2; Priority 2 "Agent Next-Steps Tool" is superseded by `next[]` (B3) and `help` (E3), which serve the plan inside responses instead of as a separate recommendation tool; Priority 2 "Search Explainability" is C7. The TODO.md items outside this plan have their own ledger records that depend on its contracts: FeatureGraph `cs-lpaz` (on A2 and B1), Invariant Miner `cs-elrq` (on A2, B1, F3), and the three research memos `cs-2i5y` (temporal property graph, on A2), `cs-ql43` (semantic time travel, on D2), `cs-3977` (architecture drift detector, on F4). Risk Biomarkers stays with `cs-sweep-health-biomarkers-5yh`. Every TODO.md item now carries a `Bead:` line, and the mapping is queryable with `br list --label todo:p1` (`todo:p2`, `todo:p3`, `todo:research`). `cs-ma4` (freshness contract) receives its concrete case in D1. `cs-4o8` (pagination) is unaffected: `completeness.truncated.recover` carries the narrowing call, not a cursor. `cs-sweep-signature-collapse-w2q` stays gated; A5 is the general mechanism. `cs-621` and `cs-lql` (resolution and import-edge precision) feed C1 and C2 but are not subsumed. `cs-q4c` (`plan_lanes`) would consume handles and `batch` once they exist.
