# Controlled Rust retrieval comparison, 2026-09-12

The previously reported Rust regression does not reproduce under matched
conditions. CodeSage at `5aaf49e` scores **0.7651**; `v0.28.0` scores
**0.7653**, a difference of **+0.0001825999** before rounding. None of the
60 paired query scores decreases. This is a single run per arm, not evidence
of a statistically meaningful improvement.

| Repository | Queries | 0.18.0 / `5aaf49e` | 0.28.0 / `8be4cbb` | Unrounded difference |
|---|---:|---:|---:|---:|
| Tokio | 20 | 0.7998 | 0.8003 | +0.0005477997 |
| Serde | 20 | 0.7883 | 0.7883 | 0 |
| Axum | 20 | 0.7072 | 0.7072 | 0 |
| Rust | 60 | 0.7651 | 0.7653 | +0.0001825999 |

Both arms completed all 60 queries with exit zero, without timeouts, degraded
reranking, CPU fallback, or transport fallback. Scores were independently
recomputed from retained paths and annotations. Fifty-nine scores are identical;
the Tokio query `async file I/O operations` improves from 0.5802792109 to
0.5912352048 as relevant `fs/mod.rs` moves from rank 8 to rank 7. Twenty-four
file rankings differ, including changes that leave NDCG unchanged. See
[paired differences](paired-differences.json) and [verification](verification.json).

## What this resolves

The [August historical result](../semble-per-language-2026-08-04-clean.json)
reports 0.7854, but that baseline does not reproduce when its recorded source
commit is rebuilt and indexed under the same conditions as the newer arm.
The [September result](../semble-per-language-2026-09-08-corrected.json)
reports 0.7653; its provenance also identifies uncommitted integrated changes,
so its label `0.27.0` alone does not identify a clean source revision.

All three Rust annotation files are byte-identical between Semble
`d899d610039d6e84de0bb2f236c5e8d75c5c5049` and the corrected revision
`a772a37d558c11bffbd99b18141705df3f2982be`. The annotation corrections cannot
explain the historical Rust difference. Hashes and counts are retained in
[annotation identity](annotation-identity.json).

The historical discrepancy remains unattributed. The August artifact contains
only aggregates and explicitly hand-backfilled provenance: no binary hash,
per-query paths, configuration snapshots, index identity, model/runtime hashes,
or ranking environment. Its earlier runner also did not classify successful-exit
stderr warnings. These gaps do not prove that a fallback or configuration change
occurred. The scoring helpers and aggregation semantics are unchanged between
the historical runner and the runner used here.

The controlled comparison rejects the proposed version-caused decrease under
the measured conditions. It does not retrospectively establish the August run's
actual conditions. No regression survives this experiment, so no ranking
bisection or product ranking change is justified by this comparison.

## Controls and provenance

The baseline is **`5aaf49ec5ffe9c61757a9827301e7250f5ec6c79`**, which reports
version 0.18.0 and is the historical artifact's stated source commit. It is
later than the `v0.18.0` tag. The other arm is the clean `v0.28.0` tag,
**`8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584`**. Both were built from clean
checkouts with `cargo build --locked --release -p codesage --features cuda`,
`CARGO_BUILD_JOBS=4`, and external target directories. Binary hashes and
first-party source paths are in [provenance](provenance.json).

Each arm received an independent extraction of the same pinned repository Git
archives, scoped to Semble's `benchmark_root`, and a fresh full index. Source
file hashes and symbol snapshots match exactly across arms. Each index contains
373/208/58 files and 3024/1147/507 chunks for Tokio/Serde/Axum respectively.
Unicode-aware chunking changes 19/1/12 chunk payloads; those are versioned
implementation differences, not reused-index artifacts. All six database
integrity checks pass. [Index comparison](index-comparison.json) retains the
logical hashes and changed chunk files.

Both arms use Jina v2 base-code and MS-MARCO MiniLM with identical pinned
tokenizer/ONNX bytes, tokenizers 0.23.1, 512-token truncation, longest-batch
padding, attention-masked mean embedding pooling, and L2 normalization.
Embedding batches are 64; reranking batches are 32. ONNX Runtime 1.24.4 is
explicitly selected with `ORT_DYLIB_PATH`. Actual `/proc` mappings captured
during each index run show identical ONNX, CUDA provider, CUDA runtime, and
driver hashes: [baseline mappings](018-loaded-runtime.json),
[newer mappings](028-loaded-runtime.json). Rust dependency changes, including
ort rc.12 to rc.13 and tree-sitter 0.26 to 0.27, remain part of the version
treatment and are recorded.

All inherited `CODESAGE_*` variables were removed, including
`CODESAGE_QUALIFIED_NAME_BOOST=1`. Only `CODESAGE_WATCH=0` and a separate
`CODESAGE_DAEMON_RUNTIME_DIR` per arm were added. No daemon was started: both
arms used private CLI inference. Shared live corpus indexes and the installed
binary were not modified. [Run receipts](018-receipt.json) and
[newer run receipts](028-receipt.json) retain UTC timing and commands.

Publication replaces the captured operator home-directory prefix with the
literal `${HOME}` token in provenance, runtime mappings, and run receipts.
All captured SHA-256 values, source revisions, query paths, scores, and query
logs remain unchanged. Those hashes identify the original measured artifacts,
not the normalized JSON documents. Recorded temporary paths describe the run;
their continued availability after cleanup is not implied.

## Reproduction

Use clean checkouts of the two exact commits above and build with the commands
recorded in `provenance.json`. The measured clean 0.28.0 binary was preserved as
`/tmp/codesage-five-eval-028-original` before an unused diagnostic build reused
its target directory; its SHA-256 matches the original clean-build receipt.

Extract each revision in [repos.json](repos.json) using `git archive` into
separate corpus roots, one per arm. Do not copy existing `.codesage` indexes.
Under each listed `benchmark_root`, create `.codesage/config.toml` using
[config-src.toml](config-src.toml) for Tokio and Axum, or
[config-serde.toml](config-serde.toml) for Serde. Inside each benchmark root,
run the corresponding binary with `index --full --no-features`.

Use the unchanged `bench/semble-ndcg-runner` from
`901b4340d1411c1f9600d94c9a1eca3cbb179634`, whose SHA-256 is recorded. For each
arm, pass its corpus root with `--corpus`, this directory's `annotations`
with `--annotations`, `repos.json` with `--repos`, the absolute binary path
with `--codesage-bin`, and an output filename with `--json`. The limit is 10.
Apply the environment policy above to both indexing and scoring. The runner's
source-derived `codesage_commit` field was corrected to each actual binary's
source commit; its original value is preserved as `runner_source_commit`.

After rebuilding and preparing the independent corpus roots, run the following
from the reviewed CodeSage repository root. Set `ORT_DYLIB_PATH` to the local
ONNX Runtime library first; the invocation checks its measured hash.

```sh
rtk proxy python3 - <<'PY'
import json
import hashlib
import os
from pathlib import Path
import subprocess
import sys

data = Path('/tmp/codesage-five-eval-data')
runner = Path.cwd() / 'bench/semble-ndcg-runner'
assert runner.is_file(), 'Run from the reviewed CodeSage repository root'
runtime = Path(os.environ['ORT_DYLIB_PATH']).expanduser().resolve(strict=True)
assert hashlib.sha256(runtime.read_bytes()).hexdigest() == 'b10825f4306b01a4951c528554f2495c4597080c63637b300669544d9c142e76'
env = {k: v for k, v in os.environ.items() if not k.startswith('CODESAGE_')}
env['CODESAGE_WATCH'] = '0'
env['ORT_DYLIB_PATH'] = str(runtime)
for arm, binary in [('018', '/tmp/codesage-five-eval-018/release/codesage'),
                    ('028', '/tmp/codesage-five-eval-028-original')]:
    env['CODESAGE_DAEMON_RUNTIME_DIR'] = str(data / f'runtime-{arm}')
    for spec in json.loads((data / 'repos.json').read_text()):
        project = data / arm / spec['name'] / spec.get('benchmark_root', '')
        subprocess.run(['rtk', 'proxy', binary, 'index', '--full', '--no-features'],
                       cwd=project, env=env, check=True)
    subprocess.run(['rtk', 'proxy', sys.executable, str(runner),
                    '--corpus', str(data / arm),
                    '--annotations', str(data / 'annotations'),
                    '--repos', str(data / 'repos.json'),
                    '--codesage-bin', binary,
                    '--json', str(data / f'{arm}-replay.json')],
                   env=env, check=True)
PY
```

Before using this invocation, verify each binary against its hash in
`provenance.json`, recreate the independent source archives and configurations
as described above, and retain the replay's diagnostics as well as its scores.
The existing runner can exit zero while reporting degraded queries; check
`warnings`, `skipped`, and every query's `status` before interpreting a result.

The annotations and repository manifest come from
[Semble at a772a37](https://github.com/MinishLab/semble/tree/a772a37d558c11bffbd99b18141705df3f2982be/benchmarks).
The upstream MIT notice is retained in [annotations/LICENSE](annotations/LICENSE).
The results use deduplicated file rankings and query-weighted language means;
they are not directly comparable to Semble's chunk-level scores. The corpus was
authored by Semble and previously exposed to CodeSage development. No comparator
was rerun, no new generalization claim is made, and no variance estimate or
latency comparison is reported.
