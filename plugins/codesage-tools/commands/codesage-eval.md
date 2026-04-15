---
name: codesage-eval
description: Evaluate CodeSage effectiveness on a project by mining session-based eval cases and running the bench runner
argument-hint: "<project-path> [--max-cases N] [--min-files N] [--no-extract] [--no-save]"
---

# Evaluate CodeSage effectiveness on a specific project

Wraps `${CLAUDE_PLUGIN_ROOT}/bin/codesage-eval`. Post-deployment effectiveness check: mine real user queries from this project's Claude Code session history, run them through CodeSage, and report how well retrieval actually performs on queries the user has asked on this codebase.

Use this AFTER `/codesage-onboard` and after the project has accumulated enough Claude Code session history (rule of thumb: at least a dozen sessions with real queries). On a freshly onboarded project with no session history, this command will fail with a clear error.

This is separate from `/codesage-bench`, which runs a regression suite across all corpora under `$CODESAGE_BENCH_CORPUS_DIR` (default: `./bench-corpora`). `/codesage-eval` builds a fresh project-specific corpus every run (unless `--no-extract` is passed).

## Step 1: Validate arguments

`$ARGUMENTS` — first positional arg is the project path. It must be a directory that exists AND has a corresponding `~/.claude/projects/<slug>/` directory with session transcripts. If either is missing, stop and explain why to the user.

## Step 2: Run the eval

```
${CLAUDE_PLUGIN_ROOT}/bin/codesage-eval $ARGUMENTS
```

The script does two things:

1. Mines up to `--max-cases` (default 50) session-based eval cases from the project's Claude Code history. Writes them to `<corpus-dir>/<project-name>-session-eval.yaml`, overwriting any previous extract unless `--no-extract` is passed.
2. Runs `codesage-bench-runner` against the fresh corpus and saves a timestamped scorecard under `<corpus-dir>/history/`.

Total runtime: usually 30-90 seconds for a small corpus on GPU. Background if it runs over 2 minutes.

## Step 3: Report the metrics

Surface the summary the script prints:

- Cases mined
- Miss rate (% of queries where no ground-truth file landed in top-10)
- Median first-hit rank
- Mean recall@5 and recall@10

Reference baselines from prior runs of this corpus (stored under the history/ subdirectory of `$CODESAGE_BENCH_CORPUS_DIR`). A reasonable healthy target across application codebases is miss rate ≤ 15% and recall@10 ≥ 0.55. If the fresh numbers regress noticeably vs a prior run on the same corpus, flag that clearly.

## Step 4: Warn about mining noise

Heuristic session mining catches queries like "fix those 2 errors" or "do not run make start" that aren't really retrieval queries. Those create false negatives (retrieval can't find files for a meaningless query, so miss_rate inflates).

After reporting the numbers, tell the user:

> Spot-check `<corpus-yaml>` before trusting these numbers. Noisy queries inflate miss_rate. If several cases look like conversational noise rather than real file-seeking questions, either edit the YAML to remove them and re-run with `--no-extract`, or tighten `--min-files` (require each case to reference at least 2-3 project files).

## Step 5: Next-step suggestions

Based on the numbers:

- **Miss rate > 25% AND queries look legitimate**: retrieval genuinely underperforms on this codebase. Candidates: more aggressive exclude patterns (via `/codesage-reset` after editing `.codesage/config.toml`), trying a different model, or accepting that the project has low retrieval affinity (thin/convention-heavy code).
- **Miss rate < 15%**: healthy. This deployment is delivering value.
- **High miss rate but queries look bad**: filter the corpus before concluding. Re-run with `--min-files 3` or hand-edit the YAML.

## Notes

- The mined corpus YAML is saved to `$CODESAGE_BENCH_CORPUS_DIR` (default `./bench-corpora`) so subsequent `/codesage-bench` runs pick it up as part of the regression suite.
- `--no-extract` reuses the existing corpus YAML — useful when you hand-edited it to remove noise and want to re-score.
- The bench runner uses `codesage search --json` directly, not the MCP. Effectiveness is measured on the underlying retrieval, not the MCP routing layer.
