# Opt-in declaration and platform penalties

Neither experiment has evidence for default-on adoption. The frozen 32-query
run retrieved 31 targets in both arms, with no first-hit rank changes. PHP
demotion changed four result pages below the first relevant hit. The platform
experiment changed no returned page on php-src.

| Category | Queries | Hits before / after | Discounted first hit before / after |
| --- | ---: | ---: | ---: |
| PHP behavior | 8 | 8 / 8 | 1.0000 / 1.0000 |
| PHP declarations | 8 | 8 / 8 | 0.8663 / 0.8663 |
| C host/shared behavior | 8 | 8 / 8 | 0.8663 / 0.8663 |
| C explicit Windows behavior | 8 | 7 / 7 | 0.6533 / 0.6533 |

Discounted first hit is `1 / log2(rank + 1)`, or zero beyond ten returned
chunks. Expected files are alternatives, not an exhaustive relevance set.
The disabled control reproduced all 32 baseline pages exactly.

## Behavior and limits

Set `CODESAGE_PHP_DECLARATION_DEMOTE=1` to apply a 0.5 multiplier to PHP
`*Interface.php` files and files under `Contracts/` or `Facades/`. Explicit
interface, contract, or facade wording, a matching filename stem, or a named
indexed symbol in the result exempts that row. Windows path separators work.
Other languages and ordinary implementation paths remain unchanged. This is
a path heuristic: a facade can contain executable behavior, so a declaration
path does not prove that the file is irrelevant.

The original bead's zero declaration targets among 95 historical PHP targets
does not imply zero regression risk. Eight independently selected declaration
queries here have legitimate declaration targets. They use explicit declaration
wording; unnamed or unindexed methods remain outside that control coverage.
No claim is made that the historical count itself was incorrect.

Set `CODESAGE_PLATFORM_DEMOTE=1` to apply a 0.7 foreign-platform multiplier.
On Unix hosts the foreign directories are `win/`, `win32/`, and `windows/`.
On Windows hosts they are `unix/`, `posix/`, `linux/`, `darwin/`, `macos/`, and
`bsd/`. An explicit foreign-platform token or portability request exempts the
page. At least one candidate in an explicitly native platform directory is
required, so a foreign-only page, including one with shared files, keeps its
scores. This conservative candidate-page condition can leave a useful demotion
inactive when native code lives under platform-neutral directories.

Both variables are off when unset; `1` or `true` enables them. They are read
once per process, so restart an existing daemon when changing them. The parent
path-penalty toggle must also remain enabled. Windows-host mirror behavior is
covered by parameterized unit tests, not by a Windows runtime benchmark.

The platform bead still requires broader non-corpus evidence before default
adoption. Only php-src was available among its named php-src, Redis, and nginx
repositories. Shared-directory targets and explicit Windows controls cannot
establish a useful implicit-host preference. The unchanged results do not
justify default-on adoption or validate the original libuv improvement outside
that corpus. Further evaluation must use a new frozen sample; these results
must not be used to tune the candidate and then reported as held-out evidence.

## Evidence and reproduction

An independent author selected and froze `holdout.json` from source without
reading candidate code or rankings. `holdout-provenance.md` records revisions,
source anchors, and limitations. The candidate was frozen before its author
opened the cases. No weight or guard changed after measurement.

`evaluate.py` reuses the full-production-pipeline runner in
`bench/qualified_name_eval.py`. Both arms compile the complete current
`search.rs`; the baseline renames only the two tuning-variable strings and
sets those renamed variables to zero. The candidate enables both experiments.
All other stages, including embedding, lexical fusion, reranking, boosts, and
anchoring, run normally. The two arms share the same CUDA query embedding and
MiniLM reranker, using disposable SQLite backups of existing indexes.
`results.json` records source and case hashes, revisions, and every returned
path; `control.json` records the disabled comparison.

The 206 distinct expected or returned files matched both structural and
semantic stored content hashes when checked against disk: 111 php-src,
42 Monolog, and 53 Laravel files. The php-src index records a Jina v2 base-code,
768-dimensional, mean-pooling CUDA fingerprint. The two older PHP indexes lack
persisted fingerprints: their configs request the same model and GPU, but
their historical execution provider and pooling identity are unverified.
This run compares rankings over those retained vectors; it does not prove
full-index freshness, model-fingerprint validation, or reindexing behavior.

Build CUDA dependencies, then pass your indexed clones:

```bash
cargo build --release -p codesage --features cuda
python3 bench/php-declaration/evaluate.py \
  --cases bench/php-declaration/holdout.json \
  --project laravel-framework=/path/to/laravel-framework \
  --project monolog=/path/to/monolog \
  --project php-src=/path/to/php-src \
  --output /tmp/path-ranking-results.json
```

Add `--control` to disable both candidates and require identical returned
pages. The runner preserves the frozen cases and accepts replacement clone
locations through the project arguments.
