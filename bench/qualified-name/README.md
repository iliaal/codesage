# Qualified-name retrieval experiment

Default-on adoption was rejected. The required mean top-10 hit improvement was
at least 2 percentage points, with a strictly positive lower confidence bound.
The frozen validation result was 0 percentage points, with a lower bound of 0.
Both arms found 129 of 130 targets. Better first-hit ranks on six queries and
worse ranks on two do not satisfy that acceptance gate.

## Resolution review, 2026-09-08

**VERIFIED ANSWER:** reject default adoption of this grouped-name candidate.
The implementation and evaluation are complete; the default-on acceptance gate
is unsatisfied. Retain the existing opt-in for reproducible investigation. This
decision does not establish that every qualified-name retrieval approach fails.

The frozen holdout has only one baseline miss. Even a perfect candidate could
improve its hit rate by at most `100 / 130 = 0.7692` percentage points, below the
required 2. Repeating or tuning against this holdout cannot clear the unchanged
gate. This ceiling limits the experiment's ability to detect a useful ranking
change; it is not evidence for lowering the threshold or shipping the candidate.

The strongest counterargument is that grouping improves lexical selectivity and
some first-hit ranks even when semantic retrieval already finds the target.
Fresh development replay supports that distinction: Laravel lexical hits rose
from 26/30 to 30/30, while complete-pipeline hits rose only from 29/30 to 30/30.
Scoped and dotted Serde lexical hits stayed at 29/30 each; their complete-pipeline
hits stayed at 30/30 each. The held-out rank regressions and zero hit improvement
still outweigh this development-only evidence under the frozen decision rule.

The next-best alternative is a separate prospective evaluation of qualified-name
resolution through indexed symbol ownership, rather than requiring every owner
component to occur in a chunk's text. The current zero-hit fallback cannot help
when a wrong chunk contains all components but the desired method chunk omits
its owner. That is a source-derived failure scenario, not a measured prevalence
claim or a new implementation commitment.

Premortem: a replacement could overvalue common owner names, miss imported aliases
or inherited methods, or mistake nearby component mentions for the requested
declaration. Another symbol-definition corpus could again saturate before it
tests those failures. Prospective cases should come from independently selected
PHP/Rust change or investigation tasks, with expected files established before
running either arm; include natural-language, dotted-name, and existing C++
controls. Keep the current holdout as a regression set, never a tuning set.

Confidence is high that this candidate has not met its acceptance gate, and low
that these near-ceiling symbol queries settle broader task value. Reverse the
default-adoption decision only after a frozen prospective task sample supports
at least 2 percentage points of mean top-10 hit improvement with a strictly
positive lower confidence bound, while its preregistered controls remain
acceptable. A new mechanism also needs its own implementation review; a better
score alone does not establish correct qualified-name resolution.

### Fresh verification

At CodeSage `8be4cbb`, the packaged CUDA pipeline runner reproduced all 90
development baseline/candidate first-hit rank pairs in `results.json`. With the
experiment disabled, all 90 complete returned file-path pages matched baseline
`136dba6` in order. Both runs used the existing Laravel and Serde revisions listed
in `results.json`, disposable SQLite backups, Jina embeddings, and the MiniLM
reranker; the runner checked the CUDA execution provider. This is a development
replay, not another independent holdout or validation of index freshness.

The compiled current query builder also produced identical default/experimental
MATCH expressions for 220 existing local evaluation queries without `::`, a
backslash, or a dot. That check covers expression preservation only, not end-to-end
natural-language retrieval. No product source, ranking weight, query annotation,
or acceptance threshold changed in this resolution review.

## Existing experiment

The default search path preserves the baseline query builder, hybrid gate,
and retrieval without the new fallback. Set `CODESAGE_QUALIFIED_GROUPS=1` only
to opt into the experiment: qualified `::`, backslash, and dotted names become
conjunctions, with selective fallback when no grouped lexical matches exist.
Unset the variable to restore the default. Other values do not enable it.

| Validation group | Queries | Hits before / after | Discounted first hit before / after |
| --- | ---: | ---: | ---: |
| Laravel qualified classes | 30 | 30 / 30 | 0.9877 / 0.9754 |
| Serde scoped methods | 30 | 30 / 30 | 0.9260 / 0.9631 |
| Serde dotted methods | 30 | 30 / 30 | 0.9587 / 0.9631 |
| Published fmtlib controls | 20 | 20 / 20 | 0.8074 / 0.8090 |
| Published abseil-cpp controls | 20 | 19 / 19 | 0.7937 / 0.8021 |

The two regressions were `Illuminate\Types\Model\Post` (rank 1 to 2) and
`fmt::arg named arguments for use in format strings` (rank 3 to 6).
Discounted first hit is `1 / log2(rank + 1)`, or zero beyond rank 10.
Rank counts returned chunks. Multiple expected files are acceptable alternatives;
this metric is not multi-relevance NDCG.

## Inputs and scope

`cases.json` freezes 60 Laravel class/interface/trait names and 60 Serde method
names from existing indexes. Generic arguments are stripped from Rust names.
Names with non-identifier components and paths under `tests/` are excluded.
The first 60 names in SHA256 order are selected per repository; even positions
are development and odd positions are validation. Queries rotate between a
bare name, a backticked implementation query, and a `how … works` query.
Dotted equivalents of the Rust queries retain the same split.

Two Laravel validation cases refer to the framework's `types/` test declarations,
including the rank regression above. They remain in the frozen sample and do
not establish application-code benefit. The synthetic query wording asks for
existing symbols; it does not model natural-language issue reports.

The 40 C++ controls are all published fmtlib and abseil-cpp annotations from
Semble, using their primary relevant files. These controls check the historical
namespace regression. No annotation or expected path was changed after measuring.
The PHP/Rust split is within repositories, so it does not test generalization
to previously unseen repositories. The dotted/scoped pairs are correlated.

Both arms compile their complete production `search.rs`: baseline `136dba6`
and the candidate working file. They share the same CUDA embedding for each
query, the same Jina model, the same MiniLM reranker, and the same disposable
SQLite backups. All search stages run, including the hybrid gate and fallback.
Source indexes remain unchanged. The experiment deliberately reuses existing
vectors to isolate search behavior; it does not validate reindexing or the CLI's
semantic-fingerprint freshness checks.

The first development experiment tried conjunctions without fallback. Its
lexical Serde hit rate fell from 29/30 to 22/30 because older chunks lacked the
qualified owner context. The retained fallback runs only when the complete
grouped lexical query has no hits. One invariant correction followed: lowercase
dotted terms must remain eligible for the legacy fallback. That correction came
from source inspection before reading validation results. The initial validation
capture is retained locally as superseded and unread; the corrected snapshot was
frozen before the reported validation run. No ranking adjustment followed those
results. After review rejected default adoption, the frozen experimental path
was retained behind the explicit default-off flag. The original default query
builder and gate were restored, and the new fallback is disabled by default.
No acceptance threshold, query, or ranking weight was changed to obtain approval.

`results.json` records source hashes, repository revisions, aggregate scores,
and every first-hit rank. The packaged runner reproduced the original A/B ranks.
After adding the opt-in guard, all 90 development default pages matched the
baseline file paths in rank order, and all 90 experimental pages matched the
frozen candidate. These checks validated the guard without retuning the holdout.

## Reproduce

Build CUDA release dependencies, then opt into the experiment explicitly in the
benchmark with your indexed clones:

```bash
cargo build --release -p codesage --features cuda
python3 bench/qualified_name_eval.py --baseline 136dba6 \
  --cases bench/qualified-name/cases.json --split holdout --pipeline --experimental \
  --project laravel-framework=/path/to/laravel-framework \
  --project serde=/path/to/serde \
  --project fmtlib=/path/to/fmtlib \
  --project abseil-cpp=/path/to/abseil-cpp
```

Use indexes built with `jinaai/jina-embeddings-v2-base-code`, mean pooling,
CUDA, and the configured `cross-encoder/ms-marco-MiniLM-L6-v2` reranker.
Use `--split development` for the 90 development queries. Omit `--pipeline`
to measure only lexical candidates. Lexical results do not measure the hybrid
gate, fusion, boosts, or reranking. Omit `--experimental` to compare the restored
default against the baseline; the runner clears any inherited opt-in variable.
