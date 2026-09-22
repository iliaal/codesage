# Diagnose a search miss

Run the same query from the indexed project directory with `--explain`. Keep the
query, limit, offset, model, and index fixed when comparing scores:

```bash
codesage status --json
codesage search --json --explain --limit 20 'where is authentication handled'
```

For MCP, call `search` with an absolute `project`, the same `query`, and
`"explain": true`. Each returned chunk has a `trace` array. Explanation is off by
default; turning it on preserves the candidate pool, scores, ordering, and cliff.

Read the trace in order. `before` and `after` are the score at the actual stage;
the last `after` equals the row's `score`. A null `before` marks admission to the
candidate pool. No-op entries explain skipped or inapplicable stages. Some reasons
group disabled and unmatched conditions; they do not establish which condition
held. Numeric evidence lives under `signals`:

| Evidence | Meaning |
| --- | --- |
| `dense_rank`, `dense_score` | One-based rank within the filtered, bounded dense pool, and similarity before fusion. A BM25-only row has no dense score. |
| `bm25_rank`, `bm25_score` | One-based rank within filtered lexical candidates and raw SQLite FTS5 score; lower raw BM25 scores are better. |
| `fused_score` | Reciprocal-rank fusion value before rescaling. The fusion entry's `after` is the score recovered from the synthetic distance used by ranking. |
| `reranker_raw`, `reranker_normalized`, `reranker_weight` | Raw cross-encoder score, min-max normalization, and blend weight. Fusion normally skips reranking; the trace records that skip. |

The remaining entries show known-symbol and declaration boosts, path penalties,
file/directory saturation, and mention anchoring. Opt-in qualified-name, stem,
version, and platform adjustments are recorded when applied. `stem_scan` identifies
a definition admitted outside dense/BM25 candidates. `symbol_annotation` reports
indexed symbols overlapping the chunk; feature ownership and graph risk do not
contribute to search ranking.

Repeated declaration matches are summarized in one `definition_boost` transition
with their count and cumulative boost. MCP budgeting keeps each surviving trace
complete. It can shorten content or symbol annotations, or drop a whole result
when its complete trace cannot fit; `completeness.kind: truncated` discloses
those cuts (`_meta` remains a deprecated alias for this release).

Use the evidence to choose the next check. Record your conclusion as a hypothesis
until the corresponding check distinguishes it from the alternatives:

| Suspected cause | Check |
| --- | --- |
| Indexing | Verify that the expected file exists and is included by indexing configuration. Compare `status --json` with a symbol lookup. An empty search page alone cannot establish that a file is unindexed. |
| Stale data | Inspect freshness in `status --json` and MCP staleness annotations. Compare returned content with the working file; reindex before drawing ranking conclusions from stale chunks. |
| Unsupported language | Check whether the file's language is supported. CodeSage parses PHP, Python, C, C++, Java, Rust, JavaScript, TypeScript, and Go. |
| Chunking | Inspect the returned `content` and line range. Check whether the needed declaration or owner context falls outside the retrieved chunk. A missing row does not reveal the boundaries of an unreturned chunk. |
| Ranking | If the expected chunk is returned, identify where its trace loses score relative to competing rows. A larger limit can expose a lower-ranked chunk, but also changes retrieval overfetch, fusion rescaling, and saturation; it is a new experiment. |
| Query wording | Try the exact indexed symbol name, a code literal in backticks, or a specific path. Compare which retrieval leg and boosts engage. A better result supports a query mismatch; it does not prove the original query had no answer. |

Traces cover returned rows, not all indexed chunks or discarded candidates. Path
filters apply after bounded dense retrieval; a missing target can therefore be a
retrieval miss before ranking begins. `confidence` measures page score separation,
not answer correctness. Automatic miss classification and `debug_search_miss` are
follow-up work.
