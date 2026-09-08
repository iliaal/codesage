# Callee-name BM25 experiment

Do not enable a callee-name field by default on this evidence. Adding an
isolated callee field found no additional targets in 170 development queries:
both arms found 169 targets in their first ten chunks. The field changed many
returned pages, so the zero gain was not an inactive experiment.

| Group | Queries | Hits before / after | Discounted first hit before / after |
| --- | ---: | ---: | ---: |
| Laravel caller probes | 40 | 40 / 40 | 0.926038 / 0.926038 |
| Laravel controls | 30 | 29 / 29 | 0.966667 / 0.966667 |
| Serde caller probes | 40 | 40 / 40 | 0.957938 / 0.959671 |
| Serde controls | 60 | 60 / 60 | 0.907123 / 0.905550 |

The caller query for `InPlaceSeed` improved from rank 4 to 3. Control queries
for `fmt::Error::custom` regressed from rank 1 to 2, and `String::deserialize`
improved from rank 6 to 2. Discounted first hit is `1 / log2(rank + 1)`, or zero
when no target appears in the first ten chunks. Overall discounted first hit
decreased slightly. These are first acceptable file metrics, not multi-file
strict recall or multi-relevance NDCG.

## What was measured

Both arms run the unchanged production `search.rs` from `136dba6`, including
its hybrid query gate, KNN retrieval, BM25 fusion, boosts, mention anchoring,
and conditional reranking. Each query shares one real CUDA Jina embedding
and the MiniLM reranker. The runner verifies identical raw KNN results at
50 candidates for every paired query. It alternates which arm runs first.

The baseline is a read-only SQLite backup of each existing index. The candidate
is a backup of that baseline with only its FTS sidecar rebuilt. Every original
row ID, content value, path, language, and line interval is preserved and
checked. A separate indexed `callees` column contains sorted, distinct
`refs.to_name` values for `kind = 'call'` within that chunk's inclusive line
interval. Import references and calls outside that interval are excluded.
The FTS tokenizer, production BM25 SQL, and tie ordering are unchanged.
SQLite's default BM25 column weights give body and callees weight 1 each.

The existing FTS document has one indexed content field; it does not have
separate name and documentation fields. This experiment isolates the proposed
callee signal. It does not implement or evaluate ripwire's complete
name×3/callee×1/doc×2/body×1 arrangement, its call resolver, or its ranking.
No production schema, default ranking, or opt-in configuration changed.

Of 1,112 Serde chunks, 772 gained a nonempty callee field. Of 12,998 Laravel
chunks, 11,573 gained one. Only 33 Serde and 1,449 Laravel chunk/callee pairs
contained names absent as exact substrings of the original body. Most call
names are already present in source text, so much of this candidate changes
term frequency and document length rather than adding new matching terms.

## Evidence limits and disposition

The 80 caller probes are synthetic: names with at least eight identifier
characters and exactly one indexed non-test caller file are selected in SHA256
order, 40 per repository. Their query template is `code that calls <name>`,
with the name backticked; their expected file comes from that existing index.
This probes retrieval of known indexed edges and is not independently labeled
user demand or proof that all real callers are indexed. Parser errors and stale
index rows remain possible. The 90 controls reuse the qualified-name
experiment's development cases. Serde's scoped/dotted controls are correlated.
These are consumed development data, not a fresh held-out evaluation.

The near-ceiling baseline limits possible recall improvement. A positive result
here would still need independent caller-oriented queries and regression
controls before shipping. The observed zero recall delta and control rank
regression do not establish a reason to change the default. Keep `cs-svo`'s
production adoption gated; reopen the experiment if independently labeled
caller searches expose misses that an extra callee field can address. This
disposition rejects this isolated candidate, not every possible weighted-field
design. No threshold or annotation was changed after seeing results.

Existing semantic vectors are deliberately reused to isolate the FTS change.
This run does not validate reindexing, incremental field maintenance, schema
migration, or CLI semantic-fingerprint freshness checks. A shippable field would
need those paths implemented and verified. Search timing excludes embedding,
includes shared caches and warmup, and is not a production latency claim.

## Reproduce

Build CUDA release dependencies, then use matching Jina 768-dimensional indexes:

```bash
cargo build --release -p codesage --features cuda
python3 bench/callee-name/evaluate.py \
  --cases bench/callee-name/cases.json \
  --output /tmp/callee-results.json \
  --project serde=/path/to/serde \
  --project laravel-framework=/path/to/laravel-framework
```

The runner clears inherited `CODESAGE_*` overrides in its search subprocess.
It writes only temporary index backups and the requested output. `results.json`
records source and case hashes, dependency artifact hashes, input snapshot
hashes, repository revisions, aggregate metrics, per-query ranks, and complete
ranked path/line/score pages. Source content is omitted from this evidence file.
Use `--freeze` with a new cases filename only to create a new development sample;
the runner refuses to overwrite an existing frozen corpus.
