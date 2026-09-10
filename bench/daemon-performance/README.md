# Daemon performance measurements

Corpus labels are publication aliases. The checked-in initial report explicitly marks its redacted source path; unredacted captures remain private. Callgrind text omits trailing whitespace without changing measurements.

Run `run.py` with Python 3.11 or newer. It uses the standard library, online SQLite backups, and real newline-delimited MCP requests over isolated Unix sockets. The runner owns its foreground daemon and measures that process through `/proc/PID/stat`; it never connects to or stops your normal daemon.

## Preserve inputs

```bash
python3 bench/daemon-performance/run.py snapshot \
  --project /absolute/project --output /tmp/codesage-perf-project-a
python3 bench/daemon-performance/run.py snapshot \
  --project /tmp/codesage-perf-project-a --output /tmp/codesage-perf-project-b
```

Outputs must not exist. SQLite backup includes committed WAL frames without checkpointing or changing the original database. The runner copies indexed files and config, rejects paths outside the project, and hashes the copied source tree. Source copying follows the database backup and is not atomic with it. Git metadata is omitted, so freshness reports `not_git`; this corpus tests indexed computation, while Git and working-file freshness require separate product tests. The second project tests scheduling isolation with identical content, not cross-corpus generality.

The runner validates database, config, and source hashes and rejects nonempty WAL files before measurement. After all owned daemons close, it repeats these checks. Mutation sets `input_validation: invalid_inputs` and retains measured results; intermediate captures remain `pending`. Keep the snapshot unchanged between compared binaries. Watcher runs use a further copy and cannot change the pinned input. Session replay writes only inside the scratch snapshot's `.codesage/sessions` directory.

## Run workloads

```bash
python3 bench/daemon-performance/run.py run \
  --binary /absolute/codesage --source-commit FULL_SHA --build-features cuda \
  --project /tmp/codesage-perf-project-a --project /tmp/codesage-perf-project-b \
  --output /tmp/codesage-perf-baseline --rounds 1
```

The default scenarios are one cold overview, 20 sequential warm overviews, 16 concurrent overviews, mixed-project load, protocol cancellation, hard disconnect, silent local timeout, and a review replay. Every scenario starts a fresh daemon; warm calls require a successful cold call on the same daemon. Concurrent clients finish their handshakes before calls start. `--concurrency`, `--timeout`, `--abandon-after`, `--observe`, and `--scenarios` control the workload and are recorded in results.

The review replay preserves 23 overview calls, 38 reference calls, and one session start from the recoverable portion of the observed 104 requests. The missing 42 calls and original timing are unavailable. The runner labels its concurrent burst and repeated `--symbol` substitution. It does not present that burst as an exact captured session.

Use `--scenarios review_bounded --concurrency 8` to replay the same 62 calls with at most eight simultaneous client calls. The original `review` scenario remains an all-at-once overload burst. Each bounded request records `client_scheduling_wait_s` separately: the MCP timeout and request `wall_s` start after that wait, while scenario `wall_to_responses_s` includes it. This bounds client calls, not baseline server work that may survive a client timeout. Successful session summaries alone do not prove snapshot-content parity; retain and compare the written snapshots separately.

Use `--models` to measure first search/model initialization and 20 subsequent searches. Warm measurement requires successful initialization. Models use the configured device and local model cache; missing artifacts may be downloaded.

Use `--scenarios model_concurrent --concurrency 16` to initialize the model successfully, then submit 16 warmed searches together. With a second `--project`, the burst also submits one `find_symbol` call using `--symbol` to measure unrelated-project interactive latency. The scenario observes post-response CPU and diagnostics for `--observe` seconds. Check native queue records, refusals, and remaining work before interpreting successful calls as capacity evidence.

Use `--watcher` to measure idle CPU and completed indexing of one randomized Rust source file in a separate scratch copy. The setup performs semantic search, which starts the watcher under default project configuration. The runner requires successful setup plus active watcher status owned by the measured daemon. Before measuring idle CPU, it waits up to `--watcher-timeout` (default 120 seconds) for explicit `startup_reconciled: true`, no pending reconciliation, and no parked work, sampled every 50 ms for at least 500 ms. This timeout is separate from the MCP `--timeout` (default 30 seconds). Older binaries without startup evidence, disabled watchers, and startup or backlog work that exceeds the timeout make idle measurement unavailable. During the idle window, any sampled pending work or missing status relabels the CPU observation `watcher_background`; it does not become an idle result. Active completion requires matching structural and configured-model semantic content hashes, 500 ms of quiescence, and at least the requested observation interval, bounded by `--watcher-timeout`. An alive watcher without those checks remains incomplete. These are sampled observations, not a continuous execution trace. All timing arguments must be finite and positive. Query and symbol strings are represented by SHA-256 in the workload manifest.

Use `--env KEY=VALUE` only for allowlisted performance controls in `PERFORMANCE_ENV`; credential variables are rejected without echoing their values. Effective allowlisted values are recorded. Unrecognized inherited `CODESAGE_*` controls are removed from the daemon environment. Other inherited environment, including credentials needed for model access, is not logged. `CODESAGE_WATCH` and the daemon runtime directory are controlled by the runner.

## Interpret results

`results.json` contains binary and input hashes, host details, environment policy, per-request outcomes and semantic hashes, per-project summaries, diagnostics before/after, and process CPU after callers depart. The runner copies the binary into its output directory and freezes its hash in the manifest. Before each launch it checks that copy against the frozen hash, then verifies the launched `/proc/PID/exe` against the same hash. Source revision and build features remain operator declarations; preserve build provenance separately. Baseline binaries without `daemon_stats` explicitly report unavailable diagnostics; their physical execution count cannot be inferred from request counts.

Wall time uses `time.monotonic()`. Individual request duration begins at send; scenario duration also includes client handshakes. Process CPU is daemon `utime + stime` divided by `_SC_CLK_TCK`, including its native threads and excluding separate child processes, the harness, and other builds. Other host load can still affect scheduling and contention. Record such load when interpreting measurements.

Each scenario measurement records `memory_before` and `memory_after` from `/proc/PID/status`. `daemon_lifetime_peak_rss_kib` is Linux `VmHWM`, the daemon's lifetime high-water RSS, not an interval peak; `current_rss_kib` is `VmRSS`. Both exclude child processes and the harness. Missing or malformed memory data is explicitly unavailable and does not discard request or CPU measurements. A warm phase's lifetime peak can include initialization.

`client_timeout` describes the observer's outcome. `abandon_action: cancel_sent` proves only that a notification was written; `transport_closed` proves a client disconnect. Neither claims that server execution stopped. Use diagnostics and the post-response CPU trace to establish physical-work lifetime. Local timeout leaves the socket open throughout the observation window.

If abandonment itself fails, `cancel_send_failed` or `transport_close_failed` preserves the timeout row and records the error class. A failed cancellation write never counts as a delivered notification or discards its concurrent peers' measurements.

If final diagnostics fail, the runner retains completed request rows and collected CPU/wall observations with `measurement_complete: false` and `failed_phase: observation`. Missing diagnostics do not erase successful responses or imply an empty workload.

Latency percentiles include successful responses only. p95 requires at least 20 successful samples; p99 requires 100. Timeouts and errors remain separately counted. Semantic hashes omit `_meta.stale_files`, `_meta.stale_warning`, the relative structural freshness summary, and the freshness-derived indexing suggestion text. Coverage, truncation, clamps, static suggestions, ranking scores/order, and other fields remain part of the hash. Product tests must exercise excluded live fields independently.

For final comparison, wait for builds to finish, then run five interleaved rounds on the same snapshots: baseline A, baseline A again, candidate B, reversing A/B order in alternate rounds. The repeated A estimates noise. Keep every result, including failures. Compare successful counts, process CPU, successful wall distributions, per-project latency, and continuation after abandonment. Compare the baseline, candidate with `CODESAGE_OVERVIEW_CACHE=0`, and cache-enabled candidate. Cache bypass retains shared graph refactors, admission limits, and cancellation; it does not isolate scheduler-only changes. Measure diagnostics overhead with `CODESAGE_DIAGNOSTICS=0` on the same candidate; absent instrumentation is not a valid zero-overhead measurement. Both controls apply when the runner starts its isolated daemon.

## Profile a complete overview

Run the immutable baseline from the snapshot directory:

```bash
valgrind --tool=callgrind --callgrind-out-file=/tmp/codesage-overview.callgrind \
  /absolute/baseline/codesage overview --json
callgrind_annotate --inclusive=yes --threshold=90 /tmp/codesage-overview.callgrind
```

Callgrind reports instruction counts, not CPU seconds. Inclusive rows overlap and must not be added. A completed CLI profile attributes the corresponding indexed computation; it does not substitute for concurrent daemon, inference, or watcher measurements.

## Initial preserved baseline

`baseline-initial.json` is a genuine preliminary capture from source `a337b5dbcdab33c682e6f21005994e5592cf8910`, built from `git archive` in `/tmp/codesage-daemon-baseline.H7CPFS` with a separate CUDA release target. Binary SHA-256: `55ac01b2cbe4ee478b120a8cc154d07558ee5b797bad35105379bbb7e78cd7db`. Database SHA-256: `4936636f4eac5ef3628bde50c03166dfb97f7d4e18ef336a58f22692a943a4dd` (197 files, 6,171 symbols, 62,668 references).

Cold overview took 2.047s and 2.02 CPU-seconds. All 20 warm calls succeeded: median 1.801s, 36.54 CPU-seconds total. All 16 concurrent calls succeeded: median 6.595s, 82.79 CPU-seconds total. After 16 clients abandoned at 50ms, the following 5s consumed 67.89 CPU-seconds for protocol cancellation, 68.69 for disconnect, and 68.65 for silent timeout.

This early capture predates the runner's explicit cancellation-action fields and input revalidation. Its `cancelled`/`disconnected` counters describe client actions, not confirmed server termination. It was collected during parallel development; host contention was not excluded. It demonstrates abandoned CPU and provides a preserved input/binary pair, but final speedup claims require the interleaved measurements above.

A completed Callgrind run on that pair recorded 14,858,710,727 instructions. `top_risk_files` covered 99.91% inclusive, dependency walks 86.68%, reference lookup 53.86%, and callee resolution 47.91%. The raw profile is `/tmp/codesage-daemon-baseline.H7CPFS/callgrind-overview-complete.out`, SHA-256 `c8f5de5b85b8b770f0e9b25265060811de803f8b87c283c864b20faacd3fb34e`. Valgrind reported a `brk segment overflow` warning; the command returned complete JSON and exit 0. Treat the warning as a profile limitation and corroborate conclusions with native process CPU.

## Completed small-corpus comparison

Five interleaved rounds completed on the preserved 197-file index and its second-root copy. Odd rounds ran A/A/B; even rounds ran B/A/A. All 15 runs finished with valid input hashes. Baseline binary SHA-256 was `55ac01b2cbe4ee478b120a8cc154d07558ee5b797bad35105379bbb7e78cd7db`; candidate SHA-256 was `63641519e18d808a5b80618382625476c180820611caeae56bb410d598d792a6`. The runner SHA-256 was `82ddd792585fb3e8d142a37b52d0298ada6d65ec4742946eb7383f86472e9ece`.

The table reports medians across ten baseline runs and five candidate runs. Latency is the median of each run's successful-request median, not a pooled percentile. CPU covers the whole scenario: one cold call, 20 warm calls, 16 concurrent calls, or 17 mixed-project calls.

| Scenario | Baseline latency | Candidate latency | Baseline CPU-seconds | Candidate CPU-seconds |
| --- | ---: | ---: | ---: | ---: |
| Cold | 1.711 s | 1.522 s | 1.70 | 1.51 |
| Warm | 1.499 s | 0.00831 s | 30.10 | 0.10 |
| Concurrent | 6.064 s | 1.667 s | 76.03 | 1.69 |
| Mixed projects | 6.491 s | 1.800 s | 85.34 | 3.53 |

Every call in those four scenarios succeeded. Each candidate concurrent run recorded exactly one ranking execution, one miss, and 15 shared joins. Successful overview semantic hashes matched across both binaries for each project, subject to the live-field exclusions above.

Over five seconds after clients departed, baseline cancellation consumed 64.29–67.02 CPU-seconds and disconnect consumed 64.38–66.11. Candidate ranges were 0–0.01 and 0–0.02 respectively. Silent timeout deliberately kept connections open: the candidate consumed another 1.45–2.21 CPU-seconds before drain, versus 63.96–66.05 for the baseline. CPU sampling has 10 ms resolution on this host; zero does not prove instantaneous cancellation.

The synthetic 62-call review burst completed all 620 baseline calls across ten runs. The candidate completed 176 of 310 and returned 134 saturation errors across five runs. Do not count this reduction in completed work as a successful review speedup. Capacity calibration remains open.

Raw results remain under `/tmp/codesage-daemon-baseline.H7CPFS/interleaved-r{1..5}-{a1,a2,b}/results.json`. No task builds or profilers overlapped the measurements; ordinary host activity was not isolated. Baseline warm scenario CPU ranged from 29.30 to 37.43 seconds, so small improvements need further evidence. The affected frontend and mobile indexes contain 2,263 and 3,237 files: this smaller-corpus comparison does not establish their cold latency, deadline suitability, or production acceptance.

## Large-index and model/watcher follow-up

Candidate SHA-256 `716d8343bc159184df1a85410b5a3cd8338bb98cbed0b3a0614b2e57c97f709e` includes bounded caller/spelling resolution reuse and unchanged semantic read-open registration. Its build source manifest was `422230239291691a35b23b4109b369ca4ae26af87443296a9d19c72493dc0f2e`; subsequent test edits are outside that manifest.

One mobile smoke run on the 3,237-file snapshot completed with valid input hashes: cold 15.779 s/15.76 CPU-seconds, 20 warm successes at 18.47 ms median/0.30 CPU-seconds total, and 16 concurrent successes at 17.160 s median/17.27 CPU-seconds total. Concurrent diagnostics show one ranking execution, one miss, 15 joins, and drained work. All 17 mixed-project calls succeeded; the frontend request took 7.811 s. Mobile semantic hashes match the completed baseline response. These are smoke observations, not a replacement for final interleaved comparisons or capacity calibration.

The matched baseline run with the same 30-second client timeout returned no successful mobile cold or concurrent responses: one cold timeout at 30.02 CPU-seconds and 16 concurrent timeouts at 472.41 CPU-seconds through the response window. Warm measurement was skipped after the failed cold request. Input hashes remained valid. This establishes a successful-completion difference on this run, not a successful-latency ratio; the earlier completed baseline profile used a longer observation timeout for attribution. Results are in `/tmp/codesage-daemon-baseline.H7CPFS/baseline-mobile-client30/results.json`.

Real CUDA initialization and 20 warm searches also completed with valid pinned input hashes. The previous profile run that mutated registration timestamps remains invalid comparative evidence. A separate real watcher run verified 5.001 s idle at 0.01 CPU-seconds and completed structural plus semantic probe indexing in 30.938 s at 0.11 CPU-seconds. That run explicitly used a 120-second timeout and predates the separate watcher-timeout option; it does not verify the corrected CLI default. CPU sampling resolution is 10 ms.

Raw results are under `/tmp/codesage-daemon-baseline.H7CPFS/{candidate-resolution-mobile,candidate-model-read-stability,candidate-watcher-completion}/results.json`.

The corrected CLI defaults were subsequently exercised in `candidate-watcher-defaults/results.json` under the same directory: MCP timeout 30 seconds, watcher timeout 120 seconds, valid pinned inputs, verified idle 5.001 seconds with no CPU increment at 10 ms resolution, and successful active completion in 30.876 seconds/0.07 CPU-seconds. Both structural and configured-model probe hashes matched.

`candidate-review-bounded-mobile-8/results.json` completed all 62 calls with eight concurrent client calls in 17.127 seconds/17.02 CPU-seconds, including successful session persistence, with valid inputs and no errors or timeouts. The retained `session-snapshot.json` matches `baseline-session-snapshot.json` from the uncached baseline CLI on every field except `session_id` and `created_at`: 3,237 files, 15,509 symbols, 32 cycles, and 50 ordered risk rows. The canonical matching payload SHA-256 is `864ab043510335996fd573c70e6171619e4c16b6fcdce4beda43845c3e31aa0b`. This proves snapshot parity, not baseline review latency: the CLI reference has no MCP client timeout and was not a timing comparison.

## Completed large-index comparison

Five interleaved rounds completed with mobile first and frontend second, 16 concurrent calls, and a 30-second client timeout. Odd rounds ran A/A/B; even rounds ran B/A/A. All 15 reports have valid, matching input hashes. Baseline binary SHA-256 was `55ac01b2cbe4ee478b120a8cc154d07558ee5b797bad35105379bbb7e78cd7db`; candidate was `716d8343bc159184df1a85410b5a3cd8338bb98cbed0b3a0614b2e57c97f709e`. Both used CUDA builds. Results remain under `/tmp/codesage-daemon-baseline.H7CPFS/large-interleaved-r{1..5}-{a1,a2,b}/results.json`.

| Scenario | Baseline successes / requests | Candidate successes / requests |
| --- | ---: | ---: |
| Mobile cold | 2/10 | 4/5 |
| Mobile warm | 35/40 | 80/80 |
| Mobile concurrent | 0/160 | 64/80 |
| Mixed projects | 9/170 | 66/85 |

Warm measurement was skipped after eight baseline cold failures and one candidate cold failure; the warm denominators exclude those skipped runs. All baseline failures were client timeouts. Diagnostics identify all 36 candidate failures as server timeouts: round 1 had three mixed-load failures; round 2 had one cold, 16 concurrent, and 16 mixed-load failures. Later successful rounds do not erase these failed measurements or establish that the 25-second server deadline is sufficient.

Successful candidate cold calls took 16.52–20.47 seconds. Across 80 successful warm calls, pooled median latency was 15.88 ms; each 20-call scenario consumed 0.24–0.27 CPU-seconds. The four wholly successful concurrent rounds consumed 16.08–21.37 CPU-seconds. Baseline concurrent response windows consumed 222.34–475.04 CPU-seconds while completing no requests. This comparison demonstrates reduced CPU through the response window and improved completion, not a successful-latency ratio or total CPU through physical drain. These scenarios had no post-response observation interval. Round 2's immediate candidate diagnostics still showed one zero-consumer execution after each failed scenario, so those snapshots do not establish its eventual drain time.

Every successful overview hash matched across binaries, subject to the live-field exclusions above:

- Mobile: 37 baseline and 209 candidate results; SHA-256 `37577899f4a2b8ad126bb19f29b2bbcc6351932b14314f275f2de91acab9461d`.
- Frontend: nine baseline and five candidate results; SHA-256 `dee6d6831ccbfdae123669ab8f02792aa107b79e8521ed901013decc0b34dbf1`.

Baseline round 2's second A run consumed 2.06 times the first A run's concurrent CPU and 2.16 times its mixed-load CPU. The campaign does not establish a host cause or uniformly small noise. Frontend coverage was one request per mixed scenario, not dedicated cold, warm, and concurrent runs. Cache-bypass, diagnostics-overhead, and queue-limit comparisons remain separate acceptance work; this campaign does not complete performance acceptance.

## Caller-import cache comparison

Three old/new pairs on the mobile snapshot compared binary `716d8343bc159184df1a85410b5a3cd8338bb98cbed0b3a0614b2e57c97f709e` with the caller-import cache binary `6f5ce0056be7dbe21680ea836afbdaa9ad9500509331691c169d6a91f8c0532d`. All six reports have valid, identical input/workload/environment manifests. Each binary completed 111/111 requests: three cold, 60 warm, and 48 concurrent. Every overview matched the mobile hash above. Results remain under `/tmp/codesage-daemon-baseline.H7CPFS/import-compare-r{1..3}-{old,new}/results.json`.

| Scenario | Median CPU-seconds, old → new | Median of run p50 latency, old → new |
| --- | ---: | ---: |
| Cold | 18.78 → 15.83 | 18.807 s → 15.851 s |
| 20 warm calls | 0.25 → 0.27 | 14.69 ms → 15.84 ms |
| 16 concurrent calls | 17.41 → 15.85 | 17.338 s → 15.785 s |

Concurrent CPU decreased in every pair, by 6.09%, 7.69%, and 22.41%; each run recorded one miss, 15 shared joins, and no remaining running or queued work. Cold CPU decreased twice but increased 5.48% in round 2. Maximum daemon-lifetime peak RSS was 148,936 KiB before and 148,972 KiB after; concurrent post-response RSS ranges were 49,268–50,744 and 49,288–51,868 KiB respectively. These samples do not establish interval peaks or leak freedom.

The cache was retained for subsequent acceptance comparisons. Three pairs without a same-build A/A control do not establish a precise causal speedup. Warm median CPU and latency increased about 8%, and the old third warm run was substantially slower than its first two. This comparison neither resolves that variability nor replaces frontend, mixed-load, or deadline acceptance.

## Native queue calibration

Eight valid reports compared native queues of eight globally/four per project (`6f5ce0056be7dbe21680ea836afbdaa9ad9500509331691c169d6a91f8c0532d`) with 16 globally/16 per project (`4efcac6d5b86bf5cb052604a3dfcb9cad99e98ef6ba04fd64cf198753b302a74`). Running native work remained limited to one globally and one per project. Each run successfully initialized real CUDA inference before a warmed search burst and one unrelated-project interactive `find_symbol` probe.

Across three 16-search bursts per variant, the smaller queue completed 15/48 searches and returned 33 saturation errors; the wider queue completed 48/48 without errors or timeouts. Wider-queue search p50 ranged from 0.736 to 0.813 seconds, and every search finished within 1.461 seconds. Scenario CPU increased from 1.15–1.66 to 2.89–3.46 seconds while completing more work; the smaller queue's lower CPU and successful latency are not equivalent-throughput wins. One four-search control per variant completed 4/4, with scenario wall time 0.411 versus 0.414 seconds and CPU 1.84 versus 2.16 seconds. That single pair does not establish an overhead bound.

All 71 successful burst searches shared SHA-256 `29968f90cd23c310d837d92c593c3c43b9e3ac86aeab5301bc19cf730ff21490`; all eight interactive probes shared `ecd86d6f834e8c502c1f100f7633ba5bb6e52d17c78a7cf6f1ee763a961b35aa`. Probe latency ranged from 18.03 to 71.51 ms. All runs had zero remaining running/queued work after the five-second observation; post-response CPU was 0–0.01 seconds at 10 ms resolution. Across the 16-search runs, lifetime peak RSS ranges overlapped: 1,219,300–1,222,260 KiB versus 1,220,592–1,221,248 KiB. Peaks include model initialization, not just queued work.

These repeated completion gains support the 16/16 queue with unchanged 1/1 execution limits. They do not guarantee admission for another native project: one project can fill the entire global queue. The unrelated probe exercised the interactive lane, not native fairness. Small sample counts and one pinned model/query do not establish general tail latency or memory bounds. Reports remain under `/tmp/codesage-daemon-baseline.H7CPFS/native-{default,wide}-{16,4}/results.json` and `native-compare-r{2,3}-{default,wide}-16/results.json` under the same directory.

## Selected-candidate frontend coverage

Five runs of selected candidate `4efcac6d5b86bf5cb052604a3dfcb9cad99e98ef6ba04fd64cf198753b302a74` completed 270/270 requests without errors or timeouts. One baseline run (`55ac01b2cbe4ee478b120a8cc154d07558ee5b797bad35105379bbb7e78cd7db`), placed between candidate rounds 1 and 2, completed 37/37. All six reports have valid inputs; frontend hashes, workload, and environment match. This is not five matched baseline/candidate pairs or a frontend A/A noise estimate.

| Scenario | Baseline CPU / successful p50 | Candidate median CPU / median run p50 | Candidate CPU range |
| --- | ---: | ---: | ---: |
| Cold | 7.77 s / 7.781 s | 7.30 s / 7.316 s | 7.05–8.45 s |
| 20 warm calls | 166.06 s / 8.158 s | 0.17 s / 10.64 ms | 0.15–0.21 s |
| 16 concurrent calls | 304.61 s / 19.726 s | 7.53 s / 7.486 s | 7.14–8.69 s |

Candidate cold latency spans 7.066–8.467 seconds and overlaps the baseline; no robust cold improvement is established. Warm and concurrent gains are large against the single baseline observation. Candidate-only mixed runs completed 80/80 frontend and 5/5 mobile calls. Mobile latency was 14.487–16.013 seconds; frontend per-run p50 was 7.687–9.212 seconds. Mixed CPU median was 23.15 seconds, range 22.42–25.26; no baseline mixed measurement was collected here.

All 37 baseline and 265 candidate frontend results match the retained frontend hash above; the five mobile results match the retained mobile hash. Each concurrent run recorded one miss/15 joins, mixed runs two misses/15 joins, and all final running/queued counts were zero. Results remain under `/tmp/codesage-daemon-baseline.H7CPFS/frontend-final-r{1..5}/results.json` and `frontend-baseline-final/results.json` under the same directory. Earlier mobile/intermediate-build comparisons remain separate evidence, not additional frontend baseline rounds.

## Structural queue calibration

Eighteen valid reports compared selected candidate `4efcac6d5b86bf5cb052604a3dfcb9cad99e98ef6ba04fd64cf198753b302a74` with smaller-queue variant `46c44446d2ebd4813c011671c6c7a523899f609461cff11eb29b896701b15e54`. Interactive queues were 64/64 versus 16/16 globally/per project; analysis queues were 32/32 versus 8/8. Both retained interactive/analysis running limits of 2/1, logical request limits of 64/32, and native queue/running limits of 16/16 and 1/1.

| Workload, three runs per variant | Selected successes / requests | Smaller-queue successes / requests |
| --- | ---: | ---: |
| 32 concurrent mobile overviews | 96/96 | 51/96 |
| 32 mobile overviews + one frontend overview | 99/99 | 51/99 |
| 16 uncached small-corpus overviews | 48/48 | 27/48 |
| 62-call review replay, client concurrency 8 | 186/186 | 186/186 |

All failures were saturation errors, not timeouts. The smaller queue rejected the unrelated frontend request in two of three mixed runs; the selected queue completed all three. Selected mobile concurrent runs consumed 14.52–15.86 CPU-seconds and completed in 14.573–15.912 seconds; mixed runs consumed 23.82–26.26 CPU-seconds. Smaller queues completed less work, so their lower CPU does not establish equivalent-throughput efficiency.

Both variants completed every bounded review, including session persistence. Selected review CPU/wall ranges were 14.21–17.80 / 14.320–18.048 seconds, versus 14.31–16.31 / 14.408–16.411 for smaller queues. Review lifetime peak RSS ranges were close: 149,148–149,676 versus 149,712–150,024 KiB. These small samples do not establish a review latency or memory advantage. All final running/queued counts were zero. Successful session responses alone do not prove parity of all six persisted snapshots; the separately retained snapshot comparison above has its own narrower scope.

The completion and unrelated-project results support retaining the selected structural queues. Raw reports remain under `/tmp/codesage-daemon-baseline.H7CPFS/structural-queues-r{1..3}-{default,small}-32/results.json`, `analysis-queues-r{1..3}-{default,small}-16/results.json`, and `review-queues-r{1..3}-{default,small}-8/results.json` under the same directory. The analysis workload explicitly disabled the overview cache; it does not measure cache-enabled single-flight throughput.

## Deadline rationale

The selected candidate retains a 25-second non-native request deadline, including admission, queueing, and execution. Across the five frontend rounds and three selected structural-queue rounds above, all 699 requests succeeded; the longest individual request took 17.511 seconds. The deadline leaves five seconds before the measured clients' 30-second ceiling for error delivery and cancellation handling. This measured envelope supports the policy for these workloads, not a universal latency guarantee. Earlier intermediate-candidate 25-second failures remain in the results and cannot count as successful performance improvements.

Native requests retain a 120-second total deadline, including queueing and initialization. This matches the existing default Hugging Face operation timeout without extending the MCP client's timeout; several downloads still share the request's total budget. Cached-model initialization and warmed bursts completed well inside that budget, but these runs do not calibrate uncached network download latency. A client with a 30-second ceiling may abandon first. Already-running ONNX/download work may outlive the logical deadline, retains its physical lease until exit, and is disclosed as continuing; 120 seconds is not a forced native-thread termination guarantee.

## Live WAL overlap

The selected `4efcac6d` binary completed a real `index --full --no-semantic --no-features` while an overview analyzed a disposable frontend snapshot. The observer retained the same running analysis execution across 28 external-commit intervals. The overview completed in 24.605 seconds and reported `_meta.ranking_recomputed: true`, with matching structured and text responses. This leaves only about 0.395 seconds under the server deadline; one successful overlap is not evidence of a reliable latency margin.

Indexing exited zero in 3.122 seconds and reported 64 files parsed with syntax errors, with zero failed files. After tracked analysis drained, a TRUNCATE checkpoint returned `[0, 0, 0]` and reduced the 174,486,152-byte WAL to zero. A follow-up overview succeeded in 11.980 seconds without recomputation disclosure. The original snapshot remained unchanged. Full indexing intentionally changed the scratch database, so these responses do not establish before/after score parity. PASSIVE observer checkpoints perturb checkpoint scheduling.

The complete private report is `/tmp/codesage-daemon-baseline.H7CPFS/wal-overlap-frontend-final/results.json`. This run verifies commit overlap, fallback disclosure, and checkpoint recovery; it does not bound WAL growth or latency for larger indexes or sustained writers.

## Corrected-candidate acceptance

Final binary `152777835e2d98b747df141568b76a828445e49cc80678d08297ad1d87ec5d6b` adds snapshot-local cycle recomputation and the missing metadata schema declaration to `4efcac6d`. Its Rust source manifest is `b8acf6629b32b9a721e9c613c293ebb0a8e09aa31c114d223c64fa6228876a31`. The real-reindex regression demonstrates why the cycle correction is required: a same-shape import edit can preserve the legacy cache token while changing cycle membership and risk scores. Ordinary risk caching remains unchanged.

Five alternating control rounds used the small pinned projects with cache and diagnostics enabled, cache bypassed, or diagnostics disabled. All 15 reports were valid and complete: 270/270 successful requests per variant, 810/810 total. Stable response hashes matched across all variants. Cache bypass retains bounded work and the graph optimizations; it is not scheduler-only.

| Scenario | Cache enabled median CPU, seconds | Cache bypass median CPU, seconds | Enabled / bypass median run p50 |
| --- | ---: | ---: | ---: |
| Cold | 0.85 | 0.82 | 0.870 / 0.828 s |
| 20 warm calls | 0.10 | 16.54 | 6.975 / 817.268 ms |
| 16 concurrent calls | 0.90 | 13.57 | 0.870 / 6.930 s |
| 17 mixed-project calls | 1.85 | 14.38 | 0.957 / 6.780 s |

Warm, concurrent, and mixed median CPU fell 99.40%, 93.37%, and 87.13% against bypass. Cold ranges overlap. Every enabled run recorded one cold analysis, no additional warm analysis, one concurrent analysis with 15 joins, and two mixed-project analyses. Bypass recorded 1/20/16/17 analyses. Diagnostics-off analysis counts are unavailable, not zero. All 60 scenario-end work snapshots drained; no continuous idle interval was measured.

Across 100 warm calls, diagnostics-enabled CPU totaled 0.47 seconds versus 0.45 disabled, an observed 4.44% difference. The 10 ms CPU tick is coarse relative to each 20-call interval: median interval CPU was 0.10 versus 0.08 seconds, and paired differences ranged from -16.67% to +42.86%. Pooled warm p95 was 9.157 versus 9.135 ms; per-run p95 ranges overlapped. This investigates the apparent median overhead above the initial 5% target but does not establish a universal overhead bound. No consistent material latency penalty justified further instrumentation changes.

One corrected-binary cold/warm/concurrent/mixed run per representative corpus completed 108/108 requests. Frontend cold took 7.883 seconds, concurrent p50 7.823 seconds, and warm p50 12.491 ms; mobile cold took 14.908 seconds, concurrent p50 15.237 seconds, and warm p50 15.789 ms. Every stable hash matched the earlier corpus-specific result. These are final-delta confirmations, not replacements for the retained multi-round comparisons.

Corrected-binary WAL overlap also passed: the overview completed in 15.942 seconds with recomputation disclosure, follow-up in 7.562 seconds, and TRUNCATE reduced the 174,486,152-byte WAL to zero. The original snapshot remained unchanged. Both WAL runs remain single-run observations with the limitations above.

Reports are under `/tmp/codesage-daemon-baseline.H7CPFS/controls-final-r{1..5}-{default,cacheoff,diagoff}/results.json`, `corrected-final-{frontend,mobile}/results.json`, and `wal-overlap-corrected-final/results.json`. The corrected runner hash is `64ad9f5032e0b08b8baca940c43f4f57bb32cf987a4e38a76d3fa0de39016e20`; WAL probe hash is `01c7a2ed447e5a4ff8151c073eb8d4227ccc6f03e84e5b4c716b022c03914f29`. WAL reports do not embed harness hashes; these identify the inspected files, not retrospective proof of earlier harness identity.

Final checks passed: full CUDA sanity gate, 55 runner tests, 17 WAL tests, and Ruff. Independent correctness, security, performance, reliability, testing, standards, API-contract, and maintainability reviews plus skeptical and red-team passes resolved the reported findings. A final cleanup removes the write-only execution counter; actual leases and diagnostic ownership are unchanged. Grouped SQL, score-only projection, ONNX thread tuning, and caller-side sharing were evaluated but not justified by the retained profiles.

The delivery build after that counter-only cleanup is `194cfe6e1b7c255ff1178b8434e31ce842fa9631c45fbb562e1a9f92cf3e0321`, Rust manifest `3db8b9aab255159936af189bffc8dd0bcd1c5c09e3b93325bfe7c8f90cf6fc24`. The full CUDA sanity gate passed again. Comparative measurements above belong to `15277783`, not this later binary; independent source review verified that the delta removes no live accounting or output behavior.

`delivery-final-smoke/results.json` records valid, complete delivery-binary checks: 54/54 ordinary calls succeeded; cancellation, disconnect, and local-only timeout each deliberately abandoned 16 calls. Over the following five seconds, daemon CPU was 0.00, 0.01, and 0.92 seconds respectively, and every endpoint work snapshot drained. Local-only timeout remains unobservable to the server, so completing its shared computation is expected; it is not evidence of immediate cancellation.

## Runner tests

```bash
python3 -m unittest discover -s bench -p test_daemon_performance.py -v
python3 -m unittest discover -s bench -p test_daemon_wal.py -v
ruff check bench/daemon-performance/run.py bench/daemon-performance/wal_overlap.py bench/test_daemon_performance.py bench/test_daemon_wal.py
```

The tests exercise real temporary SQLite WAL databases and Unix sockets. Their controlled server responses test the harness only and are not performance evidence.
