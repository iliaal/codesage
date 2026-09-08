# Semantic batch memory evaluation

Resolve `cs-b2p` by rejecting this 1 MiB prototype without changing production batching. Its soft chunk/vector budget reduced sampled peak resident memory by 3.4–3.5% on generated files and increased it on ordinary CodeSage code. The prototype also changes partial-write behavior on inference failure. That tradeoff does not justify adoption on the measured workload.

## Method

The baseline is `8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584`. Both variants use its semantic implementation, chunker, embedding backend, and SQLite writer. The prototype chunks files sequentially and flushes when retained chunk capacity plus estimated 768-dimensional vector capacity would exceed 1 MiB. A single oversized file is admitted; the next file is chunked before checking the target. This is deliberately a soft target, not a memory limit. No production source or configuration changed.

Runs on September 8, 2026 used real pinned Jina v2 base-code inference, CUDA, an NVIDIA GeForce RTX 4080 Laptop GPU, and fresh databases and processes. Both sides used the same device and inputs. The ordinary corpus contains the first 50 sorted Rust source paths at the baseline, renamed consistently; the generated corpus contains 20 roughly 512 KiB files and 30 roughly 4 KiB files of distinct Rust constants. Rust chunks receive no symbol augmentation, so these fixtures exercise the production text path without a structural symbol database.

`full` calls `semantic_full_index`; `watcher` calls `semantic_index_files` with real discovered files. These are semantic-pass measurements, not end-to-end CLI or watcher-service timings. `/proc/PID/smaps_rollup` was sampled approximately every 20 ms. RSS and private-memory peaks include model construction and result verification; short peaks between samples can be missed. Index time excludes model construction and result verification; wall time includes both. Other CPU development work occurred on this host, so timing differences are descriptive, not a throughput claim. GPU inference was serialized with other work.

## Results

All eight required corpus × entrypoint × variant runs completed successfully. Each row below is one fresh process, not a median. Raw data and retained batch accounting are in [results.json](results.json).

| Corpus / entrypoint | RSS MiB, baseline → budget | Private MiB, baseline → budget | Index seconds, baseline → budget | Wall seconds, baseline → budget |
|---|---:|---:|---:|---:|
| Ordinary / full | 1165.46 → 1174.96 | 1160.54 → 1170.00 | 17.98 → 17.83 | 21.16 → 20.96 |
| Ordinary / watcher | 1153.15 → 1173.19 | 1148.22 → 1168.13 | 18.19 → 17.60 | 21.75 → 20.89 |
| Generated / full | 1292.66 → 1249.14 | 1287.54 → 1244.15 | 106.17 → 100.50 | 114.39 → 117.61 |
| Generated / watcher | 1293.83 → 1248.01 | 1288.75 → 1242.90 | 101.01 → 102.24 | 110.63 → 109.96 |

| Corpus | Source bytes / largest file | Chunks | Stored chunk text bytes | Stored vector bytes | Largest accounted batch bytes, baseline → budget |
|---|---:|---:|---:|---:|---:|
| Ordinary | 1,701,507 / 145,588 | 1,484 | 1,881,814 | 4,558,848 | 6,440,662 → 1,041,169 |
| Generated | 10,610,740 / 524,326 | 7,180 | 10,746,210 | 22,056,960 | 32,803,170 → 1,631,225 |

Accounted batch bytes are retained chunk-string capacity plus vector capacity at the writer boundary. They exclude source buffers, chunk tuple/vector containers, allocator overhead, model allocations, and the prototype's already-read next file. Source totals are input accounting, not a measurement of simultaneous live source buffers. The baseline has up to 50 parallel source reads; the prototype reads one file at a time. Writer calls increase from one to eight ordinary batches and 21 generated batches. The large reduction in retained batch data translates to only 43.5–45.8 MiB less total sampled RSS on the generated fixture.

## Correctness and boundaries

All four matched pairs preserve chunk counts, text and line metadata, stored file hashes, and successful-pass statistics exactly. Vector bytes differ after inference regrouping. Comparing actual stored vectors with equal path sets, 768-element dimensions, finite values, and nonzero norms gives 567/1,484 changed ordinary vectors and 768/7,180 changed generated vectors. Maximum absolute component differences are 0.000149563 and 0.000166786; minimum cosine similarities are 0.999999769 and 0.999999789. These small differences do not establish a retrieval-accuracy regression, but exact vector preservation was not achieved.

A controlled `TextEmbedder` failure after 300 texts leaves zero chunks and hashes in the baseline, but 176 chunks and ten hashes in the prototype. Earlier budget groups commit before later inference fails. This test uses a fake adapter solely to expose the failure boundary; all performance and numerical results above use real inference.

The growing-file probe discovers a 17-byte Rust file, appends 11 MiB, and invokes the real `chunk_one`. It reads the resulting 11,534,353-byte source and emits 11,534,553 bytes of chunk text. Neither discovery's 10 MiB cap nor the prototype's soft target bounds this reread. A hard allocation limit would require a separate, explicit oversized-file policy and failure-semantics work. Probe results are in [controls.json](controls.json).

One preliminary budget watcher process failed with `corrupted double-linked list` after eight batch-accounting messages and before final JSON. Its corpus included a working-tree watcher edit and therefore had 1,497 chunks; it is excluded from the final table. The matched baseline succeeded, and a fresh baseline/prototype repeat on that exact preliminary corpus completed successfully on both sides. The failure location and cause remain unknown; it is not classified as environmental, pre-existing, or introduced. The [diagnostic](preliminary-native-failure.stderr) and [repeat results](native-failure-repeat.json) are retained. One partial generated run was deliberately terminated to release the GPU for another acceptance test and was rerun into a fresh database; it contributes no measurement.

## Reproduce

Use a fresh absolute scratch path. The preparation script archives the pinned dependent crate sources and generates both variants without modifying the checkout. CUDA and the pinned model artifacts must be available; CPU fallback is not enabled.

```bash
python3 bench/semantic-batch-memory/prepare.py /tmp/semantic-memory-repro
cargo build --offline --release --manifest-path /tmp/semantic-memory-repro/baseline/Cargo.toml --target-dir target
cargo build --offline --release --manifest-path /tmp/semantic-memory-repro/budget/Cargo.toml --target-dir target
python3 bench/semantic-batch-memory/run.py /tmp/semantic-memory-repro --target target/release
target/release/semantic-memory-baseline /tmp/semantic-memory-repro/ordinary failure-bound /tmp/failure-baseline.db
target/release/semantic-memory-budget /tmp/semantic-memory-repro/ordinary failure-bound /tmp/failure-budget.db
mkdir /tmp/semantic-memory-growth
printf 'fn original() {}\n' > /tmp/semantic-memory-growth/growing.rs
target/release/semantic-memory-budget /tmp/semantic-memory-growth growth-bound unused
```

Use `probe BEFORE_DB compare AFTER_DB` to compare recorded vectors. `run.py --corpus ordinary --mode watcher` narrows a diagnostic run; `--repeat 2` reverses variant order on the second repetition. Resuming a scratch directory skips recorded keys and creates fresh databases for unfinished runs. Native process failures remain failed rows; a later success does not erase them.

This evaluation closes the measured adoption question, not the general possibility of improving memory use. Concurrent projects, other model/device configurations, near-10-MiB source files, and a warmed shared daemon were not benchmarked. Reopen with a measured memory-pressure workload before extending the prototype. Preserving the original failure boundary and defining acceptable numerical equivalence would be prerequisites for adoption.

## Decision record (/think)

The strongest counterargument is that larger real skew or concurrent projects could make retained batch data a substantial part of memory use; these measurements do not establish that 50 near-cap files are safe or that byte budgeting is generally unhelpful. The next-best alternative is the current batching implementation. Confidence is high in rejecting this prototype because the failure-contract difference is directly demonstrated; confidence in extrapolating the memory result beyond these inputs is low. Reverse the decision when a representative memory-pressure workload and a parity-preserving design establish a useful measured tradeoff. The source-read growth gap remains unfixed.

Premortem: this rejection would age poorly if concurrent batches create memory pressure, near-cap generated files dominate a real workload, or readers mistake the discovery limit for a hard source-read bound.
