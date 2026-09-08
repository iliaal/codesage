# Brief efficacy evidence

Evidence date: 2026-09-08. The canary remains default-off. These observations do
not satisfy the 50-scoreable-serve threshold or establish causal efficacy.

The user approved ending this study as inconclusive on 2026-09-08 and closing
its bead as wont-fix. This is an investment decision, not evidence that serving
context is ineffective or that an adoption gate passed. Retain the default-off
canary and the measurements; further efficacy work requires a new study decision.

## Corrected production sample

The retained production ledger contains 1,215 fires: 1,198 empty, 12 repeat, and
5 served. All five served payload digests match native Claude PreToolUse
attachments in child transcripts under their parent session directories.
Searching only the parent transcript had incorrectly classified them as
unmatched. The earlier claim that these were manual probes was incorrect.

Scoring each attachment against later actions in its own child transcript gives
1 acted and 4 no-op. No ledger rows or attachments were reconstructed. The
scorer change passed 26 hook/scorer tests and independent review after two
findings were fixed.

Keep the evidence populations separate:

| Population | Scoreable serves | Observed actions | Outcome limitation |
|---|---:|---|---|
| Production sessions | 5 | 1 acted, 4 no-op | Observational; no randomized control |
| Earlier installer-comment pilot | 3 | 3 acted | ON-only; independent task acceptance failed; coupled paths were explicit task targets |
| Overview controlled pair | 1 | 1 no-op | Both model runs reached their turn limit; see below |
| Historical repair cohort, eight completed calls | 5 | 1 acted, 2 ambiguous, 2 no-op | Three native failures; only the artifact pair met full task acceptance |

These are fourteen scoreable serves across different populations, not fourteen
production serves or fourteen independent tasks. Another 36 scoreable serves
are required for observational readiness. Neither an acted label nor the
biased descriptive base rate proves that the hook caused an action.

## Overview controlled pair

Both arms started at `8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584`, with independent
clean checkouts, equivalent fresh structural/history indexes, the same pinned
binary and native hook, and identical tool permissions. The task was to reduce
overview latency while preserving output and freshness. The model was
`claude-sonnet-5`, effort `high`; each run had a $1 configured budget, 24-turn
limit, and 1,800-second deadline. The recorded random draw selected OFF first.
There were no retries.

| Result | OFF | ON |
|---|---:|---:|
| Native termination | `error_max_turns` | `error_max_turns` |
| Reported list-price usage | $0.6077492 | $0.5418616 |
| Native duration | 222.198 s | 161.773 s |
| Tool calls | 32 | 24 |
| Main-model input tokens | 48 | 48 |
| Main-model cache-created tokens | 66,384 | 59,791 |
| Main-model cache-read tokens | 1,221,426 | 1,084,248 |
| Main-model output tokens | 9,656 | 8,448 |
| Scoreable hook exposures | 0 | 1 no-op |
| Frozen-index baseline median | 4.012 s | 4.591 s |
| Resulting-patch median | 1.450 s | 1.577 s |
| Median speedup | 2.768× | 2.911× |

Both resulting patches passed the repository sanity script, CUDA release
builds, independent blind static review, and exact parsed overview JSON parity.
Timing used seven measured interleaved baseline/candidate pairs after one
warmup pair. Experiment builds had finished, and the parent agent confirmed its
build/inference work had finished and paused CPU-intensive calls for the
comparison. Each patch exceeded the predeclared
2× improvement requirement.

The pre-run review specification said, “Report incomplete/cap-exhausted runs as
failures.” Both trajectories therefore remain failed runs under that rule;
their resulting patches passed independent acceptance. The native result
reported 25 turns and the error “Reached maximum number of turns (24).” Passing
patch checks must not erase that termination, and termination must not be
misreported as broken code.

The token rows are native main-model usage; auxiliary Haiku usage is separate.
Combined reported usage was $1.1496108, including auxiliary usage. This is
model-reported list-price usage, not independently verified billed expense;
authentication used a team
subscription. One pair with two truncated trajectories cannot establish a
quality, latency, or cost benefit from the hook.

Raw native transcripts, ledgers, randomization/provenance manifests, immutable
patches, review bundles, and check output remain private local artifacts under
`/tmp/codesage-brief-pair-j3hbouMh`. The earlier failed pilot remains under
`/tmp/codesage-brief-pilot-uzst1lj_`. No raw session or environment records are
included in this repository.

## Historical repair cohort

All eight planned calls and their independent checks are terminal. Only the
artifact pair met full task acceptance (2/8 calls). The cohort is complete;
the efficacy study remains inconclusive and does not meet either adoption gate.
The failed patches described below exist only in historical experiment
checkouts; they are separate from reviewed product changes.

Four real historical repairs had independently verified parent-fail/fixed-pass
oracles before execution: artifact path identity (`ab4e925`), CMake option
sources (`a78316b`), PHP namespace impact (`196d763`), and index-lock exit status
(`82f15f2`). Each arm has a separate frozen parent checkout and fresh index,
complete ancestor history without the future fix, the same pinned native hook,
and an unchanged scorer. Hidden oracles and fixed source are excluded from
model checkouts and their build cache.

Each call has a $2 configured budget, 60-turn limit, and 1,800-second deadline;
the eight-call configured ceiling is $16. The pre-run balanced randomization
selected artifact ON/OFF, CMake OFF/ON, PHP OFF/ON, and lock ON/OFF. Original
draws and the balancing amendment are retained. No failed call was retried or
repaired after freezing its result.

| Task | Arm | Trajectory | Resulting patch | Turns | Wall time | List-price usage |
|---|---|---|---|---:|---:|---:|
| Artifact identity | ON | Completed | Passed oracle and blind review | 23 | 153.11 s | $0.5703866 |
| Artifact identity | OFF | Completed | Passed oracle and blind review | 17 | 104.19 s | $0.2553078 |
| CMake options | OFF | Completed | Failed source-preservation review | 23 | 105.48 s | $0.3009688 |
| CMake options | ON | Completed | Failed source-preservation review | 23 | 82.40 s | $0.2645270 |
| PHP namespace impact | OFF | Turn limit | Passed original oracle; failed namespace review | 61 | 434.69 s | $1.6408860 |
| PHP namespace impact | ON | Subscription limit | Unchanged parent; original oracle failed | 6 | 10.95 s | $0.0360734 |
| Index-lock exit | ON | Turn limit | Passed original oracle and blind review | 61 | 405.29 s | $1.1094200 |
| Index-lock exit | OFF | Completed | Failed original zero-wait oracle | 44 | 268.06 s | $0.9322670 |

The artifact oracle passed one test per arm. Both CMake patches passed the
original one-test oracle, but blind reviews found introduced losses of genuine
source files. Separate preservation tests passed all four cases on the frozen
parent. OFF failed two lowercase-source cases; ON also failed two trailing
uppercase-source cases. These checks enforce the frozen source-preservation
requirement; they do not replace or weaken the original oracle.

PHP OFF passed both original impact tests. Blind review rejected its file-wide
import map: imports from another namespace block, or function/constant imports,
can retarget a local class or trait dependency. Four separate preservation cases
passed on the unchanged parent and failed on OFF. PHP ON made no edits and failed
both original tests. Its terminal text was “You've hit your session limit ·
resets 5:10pm (America/Toronto)” at 17:21 UTC on September 8. The native result
had `is_error: true` and CLI exit 1 despite `subtype: success`; this is a quota
failure. The stated reset is 21:10 UTC. PHP OFF reported 61 turns with the
configured 60-turn limit and `error_max_turns`. Neither is a successful
trajectory. The quota interruption also prevents treating the PHP pair as a
clean efficacy comparison.

Before the lock pair, the user reported switching Claude accounts and resetting
quota. Local authentication inspection showed a logged-in team subscription;
account identities and credentials were not retained. This account switch is
a user-reported between-task protocol/population difference. The original
lock ON/OFF order and limits were preserved, and PHP ON was not retried.

The lock ON patch passed the original one-test CLI oracle and blind review,
but its trajectory reached the 60-turn limit (61 reported turns). Lock OFF
finished normally but returned exit 0 instead of 75 in the frozen
`--lock-wait 0` contention case. Its positive-wait behavior passed static
review; that does not override the failed original oracle. Both original
oracles compiled the exact candidate CLI after package cleanup and retained
distinct executable hashes. A possible Clippy issue in OFF's added test was
not evaluated or counted as a defect because the original oracle already
decisively rejected the patch.

| Task/arm | Tools | Main input | Cache created | Cache read | Main output |
|---|---:|---:|---:|---:|---:|
| Artifact ON | 22 | 40 | 67,581 | 1,086,988 | 8,138 |
| Artifact OFF | 16 | 32 | 28,202 | 433,254 | 5,458 |
| CMake OFF | 22 | 46 | 25,858 | 546,619 | 8,694 |
| CMake ON | 22 | 46 | 22,877 | 512,405 | 6,927 |
| PHP OFF | 60 | 120 | 104,396 | 4,264,920 | 36,896 |
| PHP ON | 5 | 10 | 4,078 | 59,417 | 674 |
| Lock ON | 60 | 120 | 70,973 | 2,739,915 | 27,616 |
| Lock OFF | 43 | 84 | 69,427 | 2,352,705 | 18,271 |

Total reported list-price usage is $5.1098366, including auxiliary usage; this
is not independently verified billed expense. Artifact ON produced three
fires (one served, two repeat); CMake ON produced five (one served, four
repeat). Their two matched scoreable serves were ambiguous and no-op,
respectively. Lock ON produced eight fires (three served, three repeat, two
empty), with one acted, one ambiguous, and one no-op. These five scoreable
serves come from three edited task trajectories, not five independent tasks.
PHP ON produced no served exposure. OFF has no enabled-hook
ledger; absence is not a fabricated zero-fire observational denominator.

Artifact oracle builds overlapped the CMake model runs, departing from the
prepared serial evaluation procedure. Later evaluator work waited for the
task pair to finish. Wall times therefore describe mixed load and cannot
establish a causal latency benefit. The small, interrupted cohort also cannot
establish a cost or quality benefit.

Initial CMake review checks reused the parent's test binary from a shared
evaluator target. The missing candidate compilation and wrong filtered-test
count exposed that error. Those apparent passes are retained as invalid.
Authoritative CMake checks cleaned the changed package before each run and
explicitly compiled the expected source root; PHP checks likewise cleaned
parser and graph packages. Artifact evaluations used separate targets and
explicitly compiled each candidate. No hidden-oracle artifacts entered the
model cache. Retained native output independently confirms all four completed
artifact/CMake arms compiled their own changed crate after their final edit.

Private provenance and eight-run records remain under
`/tmp/codesage-brief-cohort-b3PY6hBp`; evaluator evidence is referenced there.
Raw session content and environment records remain outside this repository.
