---
name: codesage-bench
description: Run CodeSage retrieval benchmarks on all corpora, save timestamped scorecards, print a summary
argument-hint: "[--corpus-dir DIR] [--no-save] [--runner PATH] [--codesage-bin PATH]"
---

# CodeSage retrieval regression check

Wraps `${CLAUDE_PLUGIN_ROOT}/bin/codesage-bench`, which runs `codesage-bench-runner` against every `*-eval.yaml` corpus in the corpus directory, saves timestamped scorecards under `history/`, and prints an aggregates table.

Corpus directory resolution (first match wins):

1. `--corpus-dir DIR` argument
2. `$CODESAGE_BENCH_CORPUS_DIR` environment variable
3. `./bench-corpora` in the current working directory (fallback default)

## Step 1: Run the benchmark

```
${CLAUDE_PLUGIN_ROOT}/bin/codesage-bench $ARGUMENTS
```

No re-indexing happens — this only runs `codesage search` against existing indexes. Each corpus takes 10-60 seconds depending on case count. Background and poll if total runtime exceeds ~2 minutes.

## Step 2: Report the summary

Parse the summary table and surface per corpus:

- Cases count
- Miss rate (% of queries with no ground-truth file in top-10)
- Median first-hit rank (1 is ideal)
- Mean recall@5 and recall@10

Flag anything off — reasonable healthy thresholds across application codebases:

- `miss_rate ≤ ~15%` per corpus
- `median_first_hit ≤ 3`
- `recall@10 ≥ 0.55`

Compare the fresh numbers against the most recent prior run of the same corpus (saved under `history/`). A deviation of more than ~5 points in r@10 on the same corpus signals a regression.

## Step 3: Identify stale corpora

If any corpus returns `FAIL` or produces unexpectedly high miss rates, check whether the eval YAML's `expected_files` still exist in the indexed project. Renames and deletions upstream stale out the ground truth and distort metrics.

Read the corresponding scorecard file under `history/` to see failed queries. Present the list to the user and offer to:

1. Update the eval YAML to remove or replace stale references
2. Leave it alone — the surprise is the benchmark doing its job

Do not silently "fix" the YAML without explicit direction.

## Step 4: Compare to the previous run (only if asked)

If the user says "compare to last run" or similar, take the two most recent scorecards per corpus in `<corpus-dir>/history/`, compute deltas on the aggregates, and report them.

Not automatic — only on request.

## Step 5: Status block

- Total corpora: `<N>`
- Total cases: `<sum>`
- Overall miss rate: `<mean>`
- Saved to: `<history dir>`
- Any flags: runner failure, threshold breach, suspected stale ground truth

## Notes

- Light regression check only — prints, saves, does not alert on thresholds automatically. User eyeballs the aggregates.
- Every run is saved to `history/`; old runs are never deleted by this command.
- The runner calls `codesage search --json` directly; it does NOT go through the MCP. This is intentional — benchmarks should exercise the retrieval pipeline without the MCP routing layer in the way.
