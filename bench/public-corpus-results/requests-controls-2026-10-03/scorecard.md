[results-json] wrote 20 records to /tmp/codesage-goal-20261003-retrieval-controls/requests-results-r4.json
# CodeSage retrieval scorecard

- **Project**: `/tmp/codesage-goal-20261003-retrieval-controls/requests/src/requests` (HEAD `ef439eb779c1eba7cbdeeeb302b11e1e061b4b7d`, 18 files, 5,625 LoC)
- **Corpus**: `requests-corpus.yaml` — 20 cases, top-10, k=[5, 10]
- **CodeSage**: 0.38.0 (release)
- **Embedding model**: jinaai/jina-embeddings-v2-base-code (gpu)
- **Reranker**: cross-encoder/ms-marco-MiniLM-L6-v2
- **Baseline for comparison**: placebo+rg
- **Run at**: 2026-10-03T16:52:04Z

**Measured quantities**: miss rate (no ground-truth file in top-N), median first-hit rank, mean recall@5, mean recall@10, mean tokens to first relevant hit (cl100k_base, when tiktoken is installed; misses are charged a 32k penalty so the average isn't dominated by a single un-bounded outlier). Per-case rows show `first_hit_rank`, `noise_before_first_hit`, recall@5, recall@10, tokens-to-hit, and the ranked list length after any refs-mode exclusion.

**Not measured** (flagged so nothing reads in that we didn't test): agent tool-call counts, end-to-end task-completion rate, wall-clock vs grep/ripgrep. Add those separately when claiming them.
- **Executed controls**: seeded uniform-file placebo (seed 0), `rg --sort path` literal query-token OR, and rg at the CodeSage byte budget.
- **Byte unit**: UTF-8 path + LF + content + LF, all emitted chunks charged; duplicate files and refs-mode exclusions consume bytes. Final control row may be cut; a file counts only after its full path header fits.
- **Actual semantic fingerprint**: `v4;model=jinaai/jina-embeddings-v2-base-code;artifacts=3a89def207904f28854caa8d38685ce1fa9926e3b790c3bba2a15cba6e9a57d7;dim=768;pooling=mean;device=cuda;ort=api1.24/dylib;pipeline=1;maxseq=1024;norm=l2;chunker=3;chunk=1500/350/200`
- **Eligible files**: 18, SHA-256 `8f75149b4ea06918d13f235e9f892371755435a181c662e5d9c6c269f8806d6c`
- **Instrument**: current-state retrieval; fresh private inference per query, with initialization logs and no daemon reuse. Indexing pass mode and prospective change prediction are not measured.

## Per-case results

| id | src | first hit | noise | r@5 | r@10 | tokens→hit | returned |
|---|---|---:|---:|---:|---:|---:|---:|
| requests-00 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 326 | 6 |
| requests-01 | semble:semantic | 2 | 1 | 1.00 | 1.00 | 694 | 7 |
| requests-02 | semble:semantic | 1 | 0 | 1.00 | 1.00 | 327 | 6 |
| requests-03 | semble:semantic | 2 | 1 | 1.00 | 1.00 | 770 | 7 |
| requests-04 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 322 | 8 |
| requests-05 | semble:semantic | 1 | 0 | 1.00 | 1.00 | 339 | 7 |
| requests-06 | semble:semantic | 1 | 0 | 1.00 | 1.00 | 345 | 6 |
| requests-07 | semble:semantic | 1 | 0 | 1.00 | 1.00 | 296 | 8 |
| requests-08 | semble:semantic | 1 | 0 | 1.00 | 1.00 | 186 | 7 |
| requests-09 | semble:semantic | 2 | 1 | 1.00 | 1.00 | 621 | 9 |
| requests-10 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 345 | 6 |
| requests-11 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 398 | 8 |
| requests-12 | semble:architecture | 2 | 1 | 1.00 | 1.00 | 705 | 9 |
| requests-13 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 339 | 5 |
| requests-14 | semble:architecture | 1 | 0 | 1.00 | 1.00 | 346 | 6 |
| requests-15 | semble:symbol | 1 | 0 | 1.00 | 1.00 | 365 | 7 |
| requests-16 | semble:symbol | 1 | 0 | 1.00 | 1.00 | 419 | 8 |
| requests-17 | semble:symbol | 1 | 0 | 1.00 | 1.00 | 345 | 5 |
| requests-18 | semble:symbol | 1 | 0 | 1.00 | 1.00 | 332 | 6 |
| requests-19 | semble:symbol | 1 | 0 | 1.00 | 1.00 | 356 | 9 |

## Paired controls

| id | real r@10 | placebo r@10 | rg r@10 | rg matched r@10 | real B | placebo B | rg B | rg matched B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| requests-00 | 1.000 | 0.000 | 0.000 | 0.000 | 15548 | 15548 | 32436 | 15548 |
| requests-01 | 1.000 | 0.000 | 1.000 | 1.000 | 16102 | 16102 | 18915 | 16102 |
| requests-02 | 1.000 | 0.000 | 1.000 | 1.000 | 15259 | 15259 | 23970 | 15259 |
| requests-03 | 1.000 | 1.000 | 1.000 | 1.000 | 15863 | 15863 | 32186 | 15863 |
| requests-04 | 1.000 | 0.000 | 0.000 | 0.000 | 15470 | 15470 | 14904 | 14904 |
| requests-05 | 1.000 | 0.000 | 1.000 | 1.000 | 16075 | 16075 | 31255 | 16075 |
| requests-06 | 1.000 | 1.000 | 0.000 | 0.000 | 14188 | 14188 | 57578 | 14188 |
| requests-07 | 1.000 | 1.000 | 1.000 | 1.000 | 13738 | 13738 | 4842 | 4842 |
| requests-08 | 1.000 | 1.000 | 1.000 | 1.000 | 15207 | 15207 | 4486 | 4486 |
| requests-09 | 1.000 | 0.000 | 0.000 | 0.000 | 15627 | 15627 | 45377 | 15627 |
| requests-10 | 1.000 | 1.000 | 0.000 | 0.000 | 16079 | 16079 | 188164 | 16079 |
| requests-11 | 1.000 | 1.000 | 1.000 | 1.000 | 16135 | 16135 | 51758 | 16135 |
| requests-12 | 1.000 | 0.000 | 0.000 | 0.000 | 15267 | 15267 | 85811 | 15267 |
| requests-13 | 1.000 | 1.000 | 0.000 | 0.000 | 16533 | 16533 | 41401 | 16533 |
| requests-14 | 1.000 | 0.000 | 1.000 | 1.000 | 15330 | 15330 | 33964 | 15330 |
| requests-15 | 1.000 | 1.000 | 1.000 | 1.000 | 14079 | 14079 | 2662 | 2662 |
| requests-16 | 1.000 | 0.000 | 1.000 | 1.000 | 15199 | 15199 | 123 | 123 |
| requests-17 | 1.000 | 0.000 | 1.000 | 1.000 | 14614 | 14614 | 2471 | 2471 |
| requests-18 | 1.000 | 0.000 | 1.000 | 1.000 | 15589 | 15589 | 1231 | 1231 |
| requests-19 | 1.000 | 1.000 | 1.000 | 1.000 | 16393 | 16393 | 10614 | 10614 |

| arm | miss rate | median first hit | recall@5 | recall@10 | NDCG@10 | mean B |
|---|---:|---:|---:|---:|---:|---:|
| codesage | 0.000 | 1 | 1.0000 | 1.0000 | 0.9262 | 15414.7500 |
| placebo | 0.300 | 10 | 0.1500 | 0.4500 | 0.2246 | 15414.7500 |
| rg | 0.000 | 7 | 0.4000 | 0.6500 | 0.3313 | 34207.4000 |
| rg_matched | 0.300 | 5 | 0.4000 | 0.6500 | 0.3313 | 11466.9500 |

Real minus placebo recall@10: +0.5500; wins 11/20. Clears the predeclared majority-of-cases ranking check.

## Aggregate
- **Miss rate (no ground-truth file in top-10):** 0%
- **Median first-hit rank (hits only):** 1
- **Mean recall@5:** 1.00
- **Mean recall@10:** 1.00
- **Mean tokens to first relevant hit (cl100k_base, miss penalty 32,000):** 409

## Quotable one-liner
> CodeSage 0.38.0 (release) hits 100% of 20 ground-truth cases on `requests` (HEAD `ef439eb779c1eba7cbdeeeb302b11e1e061b4b7d`, 18 files, 5,625 LoC) with jinaai/jina-embeddings-v2-base-code + cross-encoder/ms-marco-MiniLM-L6-v2, median first-hit rank 1, mean recall@10 1.00. Baseline for comparison: placebo+rg. Matched-byte placebo recall@10 0.45, delta +0.55, wins 11/20; rg recall@10 0.65 (matched-budget 0.65). Run 2026-10-03T16:52:04Z.

<!-- METRICS: miss_rate=0.0000 median_first=1 r5=1.0000 r10=1.0000 cases=20 project=requests head=ef439eb779c1eba7cbdeeeb302b11e1e061b4b7d model=jinaai/jina-embeddings-v2-base-code reranker=cross-encoder/ms-marco-MiniLM-L6-v2 baseline=placebo+rg run_at=2026-10-03T16:52:04Z codesage=0.38.0 build=release mean_tokens_to_hit=409 search_failures=0 placebo_r10=0.4500 rg_r10=0.6500 rg_matched_r10=0.6500 placebo_delta=0.5500 placebo_wins=11 control_seed=0 -->
