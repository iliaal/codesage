# Current retrieval generalization evidence, 2026-09-08

**Conclusion: current generalization to unseen repositories is unmeasured.**
The inspected data does not establish an unspent repository-held-out split.
No current-versus-simpler-ranker comparison was run for this report, and no
acceptance verdict or per-repository delta is claimed. This is the explicit
insufficient-data outcome allowed by `cs-cug`, not evidence that either ranker
is better, worse, or equivalent.

## Data and prior exposure

The retained [2026-08-04 result](semble-per-language-2026-08-04-clean.json)
contains 33 repositories and 663 queries. Its provenance says it was backfilled
by hand and identifies CodeSage 0.18.0, code state `5aaf49e`, and Semble
`d899d610039d6e84de0bb2f236c5e8d75c5c5049`. It remains historical evidence;
this report neither changes those scores nor recasts them as a current run.

The local Semble mirror inspected at
`a772a37d558c11bffbd99b18141705df3f2982be` has 63 repositories in
`benchmarks/repos.json` and 63 annotation JSON files. Exactly 33 manifest
repositories use CodeSage-supported languages. Their names equal both the
33 directories under `~/.cache/semble-bench` and the historical result's
33 repository keys. No additional supported-language repository was found
in that manifest. The other 30 cannot serve as a matched evaluation of
CodeSage's supported retrieval behavior.

Prior measurement alone would not prove every repository influenced tuning.
Here, `crates/graph/src/search.rs` also records selection using pooled Semble
results for `stem_match_boost_enabled` and `fused_rerank_enabled`, and explicit
directory-saturation tuning on Laravel, Redux, and Flask. These records prevent
treating the already inspected corpus as a sealed final holdout. They do not
prove overfitting or invalidate the historical measurements.

The configured `CODESAGE_BENCH_CORPUS_DIR` was also inspected, along with
`bench/corpora` and `bench/history`. These contain existing evaluation and
development corpora, but no repository-held-out assignment or consumption
record was found. Their unspent status is unknown; absence of a record is
insufficient to certify them as fresh data. No private corpus contents or
scores are published here.

Concurrent qualified-name work reported using Laravel and Serde for
development and within-repository validation, with fmtlib and abseil-cpp as
regression controls. Those four repositories are explicitly excluded from
any claim of a fresh repository holdout. That work's within-repository split
does not answer this report's cross-repository question.

## Existing protocol and its limits

The closed `cs-ssm` bead describes a "repo-disjoint, salted held-out split"
and a strict@10 acceptance rule with latency/token ceilings. The actual
implementation inspected in the original `841a38e`, reviewed worktree tip
`15dfdf4`, and current `136dba6` assigns **cases** using
`sha256(salt + "\0" + case_id)`, not repositories. Current
`codesage-bench-runner` applies that function to each case's `id`.
`compare-runs.py --cluster-key repo` groups the bootstrap; it does not change
split assignment or enforce disjoint repositories. A read-only probe with
salt `protocol-audit` assigned IDs `0` and `1` to train and `2` and `3` to
heldout regardless of their repository. This probe ran no retrieval queries.

The current comparison gate checks mean recall@10 improvement of at least
2 percentage points, a positive bootstrap lower bound, MRR and miss-rate
regression limits of 0.5 points, and a 2-point per-cluster recall regression
limit for clusters with at least five cases. It refuses fewer than five
bootstrap clusters. It does not implement the bead description's strict@10
metric or latency/token ceilings. These differences are recorded so an
eventual run cannot silently claim the stronger protocol.

No harness was rebuilt and the closed bead was not reopened. Resalting seen
cases would not erase exposure; a case split or repository bootstrap would
not establish the required repository separation.

## Provenance of this audit

The checkout was `136dba6` with concurrent uncommitted product changes.
The three harness files below and the historical JSON were byte-identical
to that commit when inspected. No binary was executed for retrieval, so
there is no measured binary hash, runtime configuration, latency, or current
score to report.

| File | SHA-256 |
|---|---|
| `bench/compare-runs.py` | `541bf04e1a70e9362745487b7f12db61e4586942b51f3a40ba9eae764f6e32b1` |
| `bench/codesage-bench-runner` | `a5725a5ccf773717b845c7a70c64a8243a4f4441eca7256d454bb30cff0f496c` |
| `bench/semble-ndcg-runner` | `2090231b62c41990829059e69bdb78024401c649b46d5ae1ecb3e19d533e7c45` |
| `bench/public-corpus-results/semble-per-language-2026-08-04-clean.json` | `1bf1cede7bf4f53ce9fd1b725d503f2e315bd92d6bed676cbd3e7cb27a0a4001` |

## Evidence needed to change the conclusion

Obtain a verifiably unspent, repository-disjoint dataset before evaluating
the final arms. Freeze the current ranker, a specified simpler baseline,
repository assignments, labels, and acceptance criteria before inspecting
held-out results. Reuse the existing runner and comparison machinery where
their recorded semantics match those criteria; preserve explicit repository
assignments outside the case-level splitter. Capture both actual binary
hashes, source states, configuration and environment, repository revisions,
index/model provenance, and matched execution conditions. Retain all paired
records, failures, per-repository regressions, and the one-time holdout
consumption record. Do not tune either arm against those results.
