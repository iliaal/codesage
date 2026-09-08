# Overview risk-walk reuse

On September 8, 2026, request-local walk reuse reduced median `codesage overview
--json` latency from 4.5001 s to 1.4688 s (3.06×) on a snapshot of CodeSage's
index: 189 files, 5,941 symbols, and 60,360 references.

| Round | Baseline seconds | Candidate seconds | Order |
| --- | ---: | ---: | --- |
| 1 | 4.1906 | 1.4688 | Baseline, candidate |
| 2 | 4.5001 | 1.3883 | Candidate, baseline |
| 3 | 5.0509 | 1.6995 | Baseline, candidate |

Both binaries ran as fresh CLI processes against the same SQLite backup under
`/tmp/cs-7ik-project/.codesage/index.db`. Every run produced identical complete
overview JSON. The snapshot retained the existing index's freshness state;
this measures query execution, not indexing. No inference or Cargo build ran
during these three pairs. Other desktop processes remained active. Earlier
pairs overlapped compilation and are excluded from the table.

Baseline source: `8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584`. Both binaries used
the CUDA-enabled release configuration. The candidate changed only batch risk
walk reuse on the overview path.

| Artifact | SHA-256 |
| --- | --- |
| Baseline binary | `7cf5cc45d1920179987ffead8e24c1dd044b6354ef1b5382f067eea012218e15` |
| Candidate binary | `13f5e7a35c998c59faea769e4e187734aa66a6e33100dc69d50bb55f2c7f6ee4` |
| SQLite backup | `935862dfe318cff49a095d84268a7bf2be8be693da523925b5ef2eb3e31ff16d` |

A partial Callgrind capture of the baseline attributed 88.74% of recorded
instructions to dependency walks, including 46.54% to import-target resolution
and 22.56% to loading file-import references. These inclusive percentages
overlap. The profiler was stopped after establishing the cause; they describe
the captured interval, not a completed run or wall-clock proportions.

`assess_risk_batch` now shares the existing bounded `WalkCache` across its files.
Each request starts with an empty cache. The scoring formula, frontier admission,
candidate cap, and error handling remain unchanged. No persisted score cache or
new invalidation protocol is needed.

Run the snapshot parity probe with your own index backup:

```sh
CODESAGE_OVERVIEW_BENCH_DB=/absolute/path/index.db cargo test -p codesage-graph --lib git_history::risk::tests::measure_batch_risk_on_index -- --ignored --nocapture
```

The probe compares complete batch reports against uncached assessments at the
same author-concentration timestamp. Separate CLI `risk-batch` invocations on
all 189 files also matched outside author concentration, whose `as_of` and
time-decay rounding differed between invocation times. Unit coverage includes
mixed PHP/Python/Rust names, duplicate inputs, missing files, capped frontiers,
and structural/history updates between requests.

This single-project result does not establish a cross-repository latency bound.
