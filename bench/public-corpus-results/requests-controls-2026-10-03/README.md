# Requests retrieval controls, 2026-10-03

CodeSage recall@10 was 1.0000, versus 0.4500 for the seeded placebo at exactly the same path/content byte budget. The paired delta was +0.5500; CodeSage won strictly on 11/20 cases and tied on nine. This clears the runner's predeclared strict-majority check for this corpus and seed. It does not establish an advantage across repositories or languages.

| Arm | Miss rate over its emitted page | Median first hit, hits only | Recall@5 | Recall@10 | NDCG@10 | Mean bytes |
|---|--:|--:|--:|--:|--:|--:|
| CodeSage | 0.0000 | 1 | 1.0000 | 1.0000 | 0.9262 | 15,414.75 |
| Placebo | 0.3000 | 10 | 0.1500 | 0.4500 | 0.2246 | 15,414.75 |
| rg floor | 0.0000 | 7 | 0.4000 | 0.6500 | 0.3313 | 34,207.40 |
| rg at matched budget | 0.3000 | 5 | 0.4000 | 0.6500 | 0.3313 | 11,466.95 |

The current protocol-v2 run started at `2026-10-03T16:52:04Z`. All 20 CodeSage searches and controls completed without errors. Source, config, binary, artifact, and index pins agreed before and after the run. Every placebo consumed exactly its paired CodeSage byte budget. The full rg page can name gold after rank ten; that explains its zero miss rate alongside recall@10 of 0.6500. rg can exhaust its matches before reaching the matched budget.

This fresh run follows private-runtime captures from `2026-10-03T15:59:27Z` and `2026-10-03T15:28:57Z`, and the initial protocol-v1 capture from `2026-10-03T14:48:30Z`, whose selected runtime was not attested. Those captures remain separate and have not been given retroactive evidence. The current instrument resolves executable identities once before any child executes. Its runner SHA-256 is `dc685d0817d5caea84c35284df53e48deb0f1cf88600544fa637bbcaa2b2300f`, and its helper SHA-256 is `62e230289a800d710d7132567a7744f92b4ca4af3ab26dce613b51b043c58f8c`. The helper retains `rg --no-config` and Git child isolation for the product's 15 repository selectors, while preserving configuration hardening and `GIT_NO_REPLACE_OBJECTS`. The new run reproduced all 20 CodeSage stdout hashes and all four arms' hit orders and byte costs. It explicitly used `/usr/bin/rg`, SHA-256 `c7b19d09729dca815896b47ccf3f7beca13af202fcb20801034f197a2474ace2`; the initial run used a different vendored rg binary. These are separate instruments even though these measurements agreed.

## Instrument

The published JSON replaces local home paths with `/redacted-home` and records the original capture's SHA-256. Measurements, content hashes, runtime stderr, and artifact byte pins are unchanged; the original capture is retained privately.

The [corpus](requests-corpus.yaml) contains all 20 Requests queries from [semble annotations at a772a37](https://github.com/MinishLab/semble/blob/a772a37d558c11bffbd99b18141705df3f2982be/benchmarks/annotations/requests.json), with `relevant` files as binary gold and `secondary` files excluded. Annotation SHA-256 is `212684430a5199320215b782780368561418f2efd1b7295dfc92ed32888ef586`. Paths are made relative to the declared `src/requests` benchmark root. The [annotation license](LICENSE) is retained.

Requests source revision is `ef439eb779c1eba7cbdeeeb302b11e1e061b4b7d`. A disposable local clone was indexed at `src/requests`, with 18 eligible source files and 150 semantic chunks. Existing benchmark indexes were not modified. The run used installed CodeSage 0.38.0, CUDA, Jina v2 base-code, mean pooling, and the derived fp16 MiniLM reranker. The binary SHA-256 is `f1de6127931f8418aaab7748a91c8daed4dc48a81e585cb07059bf5b0aa4ddd8`; binary/source byte identity was not attested.

The actual persisted embedding fingerprint is:

```text
v4;model=jinaai/jina-embeddings-v2-base-code;artifacts=3a89def207904f28854caa8d38685ce1fa9926e3b790c3bba2a15cba6e9a57d7;dim=768;pooling=mean;device=cuda;ort=api1.24/dylib;pipeline=1;maxseq=1024;norm=l2;chunker=3;chunk=1500/350/200
```

[results.json](results.json) retains the actual model/runtime artifact byte pins, reranker artifact byte pins, eligible file list, source hashes, semantic file/chunk digests, runner hashes, canonical ranking/model environment, seed, and per-case arm scores and byte costs. Reranker graph/tokenizer selection was checked against the 0.38 loader pins in `crates/embed/src/model.rs`; the CLI does not expose a separate loaded-reranker fingerprint. The tokenizer and CUDA graph hashes are `d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66` and `32eef6d63e978aba96b6c4134b93b252eab2eb6ae3f8b1b5d15c96b4473f7eb2`.

Protocol `retrieval-controls-v2` forces a fresh empty mode-0700 `CODESAGE_DAEMON_RUNTIME_DIR` for each search process. The existing CLI selects private inference when that directory has no resident socket; it does not start a daemon for search. Each case retains the inference PID, runtime path, empty directory listings before and after execution, complete successful stderr, and its SHA-256. The runner requires private embedding initialization and, when configured, private reranker initialization; daemon-use messages or runtime directory entries invalidate the case. The actual run recorded 20 distinct PIDs and directories, empty listings, `execution_provider="cuda"`, and private reranker initialization in every case. Thus the caller's binary and verified on-disk model pins govern new private sessions. This does not attest historical daemon sessions.

The only recorded ranking/model environment override is `CODESAGE_QUALIFIED_NAME_BOOST=1`. That boost is default-off in the measured binary. All other overrides in the canonical environment are absent. The runner excludes the ignored parent daemon directory and `CODESAGE_BENCH_CORPUS_DIR` from compatibility, records every other `CODESAGE_*` setting plus ONNX/Hugging Face cache overrides, and pins model/runtime artifact bytes separately. It sets `RUST_LOG=warn,codesage=info` and `NO_COLOR=1` in each query child to retain readable evidence.

The byte unit is UTF-8 `path + LF + content + LF`, shared by all arms. It measures the retrievable path/content payload, rather than JSON syntax, scores, or envelope metadata; raw CodeSage stdout bytes are recorded separately. Every emitted CodeSage chunk consumes budget, including duplicates and refs-mode exclusions. File scores deduplicate in emission order. A truncated control row names a file only after its complete path header fits; any remaining content is charged in bytes.

The placebo uses seed 0 plus the case ID to shuffle all eligible semantic files uniformly and choose one actual indexed chunk per file. If one round is too short, it draws another shuffled round; repeated files consume bytes and do not create new ranked files. It never consults the query or gold. rg executes case-insensitive fixed-string OR over the query's distinct ASCII identifier tokens, with paths sorted and restricted to the same eligible universe. Every command includes `--no-config`, so ambient `RIPGREP_CONFIG_PATH` cannot change the floor. Its full page and its page cut at the CodeSage budget are both scored. This is one reproducible literal-search policy, not an optimized human search strategy.

This instrument measures current-state known-item retrieval with daemon reuse prohibited. It does not measure prospective change prediction, indexing pass mode, indexing latency, or agent task completion. The 20 questions and one seed give a bounded check, not a sealed holdout or a population estimate. The 2026-09-08 nine-language table has no measured placebo delta and remains historical; this run does not reconstruct or certify that table. The strict validator recomputes first-hit rank, recall, MRR, NDCG, noise, and returned-file count from retained hits and gold. Optional token costs derive from content that the JSON does not retain, so file-list validation does not independently attest token costs.

## Reproduce

Use Python 3.11 or newer with PyYAML installed. `tiktoken` adds optional token costs; byte budgets and retrieval scores do not require it.

Use a separate Requests checkout at the source revision above. At its `src/requests` directory, run `codesage init`, set the config below, and run `codesage index --full` with your chosen CUDA binary. Do not copy an older retained index.

```toml
[project]
name = "requests"
[embedding]
model = "jinaai/jina-embeddings-v2-base-code"
device = "gpu"
pooling = "mean"
reranker = "cross-encoder/ms-marco-MiniLM-L6-v2"
[index]
exclude_patterns = []
```

Run from your CodeSage checkout, substituting the absolute source, binary, rg, and cached artifact paths. The runner resolves executable arguments once against your caller directory and `PATH`, then uses those absolute paths for version, status, search, rg, and byte pins. Before reproducing this configuration, unset all other `CODESAGE_*`, `ORT_DYLIB_PATH`, `HF_HOME`, `HF_HUB_CACHE`, and `HUGGINGFACE_HUB_CACHE` overrides. A parent daemon-directory or corpus-directory setting is harmless because the runner overrides or ignores it. Changing ranking settings, artifacts, source, or instrument bytes produces a new measurement.

```sh
env CODESAGE_QUALIFIED_NAME_BOOST=1 python3 bench/codesage-bench-runner \
  bench/public-corpus-results/requests-controls-2026-10-03/requests-corpus.yaml \
  --project-root /absolute/requests/src/requests \
  --codesage-bin /absolute/codesage \
  --rg-bin /absolute/rg \
  --limit 10 \
  --control-seed 0 \
  --reranker-artifact /absolute/tokenizer.json \
  --reranker-artifact /absolute/onnx/model_cuda_fp16.onnx \
  --results-json /tmp/requests-controls.json
```

Select the reranker artifacts from `IA0x00/ms-marco-MiniLM-L6-v2-codesage` revision `3a8dae18b7a92308d7d63b834f3bfa01bab02d32`. The runner verifies source hashes against the semantic index, verifies its persisted model/runtime artifact digest, and refuses changed provenance. `compare-runs.py` requires protocol-v2 runtime evidence and refuses omitted/invalid control arms, contradictory file metrics, or incompatible corpus, source, universe, chunk, fingerprint, model, reranker, device, artifact, ranking-environment, runner, or control-helper pins. Each paired case retains and compares its originating input-file provenance; compatible multi-project runs may use different file orders and partitions. Swapping source or model pins between case groups is refused. It rejects controlled/legacy concatenation in either order and requires controlled records on both sides of a controlled comparison. All-legacy comparisons remain available with an uncontrolled-input disclosure. Compatibility requires identical runner/helper SHA-256 values; v1 has no implicit upgrade path. An intentional `--allow-mismatch` override names each affected case, input file, differing field, and value. Candidate binary bytes may differ, with their version provenance displayed.

Required pins must contain evidence: SHA-256 digests, versioned semantic fingerprint fields, absolute file paths with positive sizes and digests, a nonempty source manifest and eligible universe, and tokenizer/ONNX artifact pins. The comparator checks retained manifest digests, the combined embedding artifact digest, agreement between metadata, fingerprint fields, and query reranker selection, and that gold and returned files belong to the pinned universe. Modern input also requires a Boolean completion flag; `--allow-partial` accepts an explicitly unfinished run. Missing, null, empty, or malformed required evidence is refused even with `--allow-mismatch`. An unsplit run legitimately records `split: null` and `salt: null`; no reranker records `reranker: "none"` and an empty artifact list; an empty environment map records no overrides. A non-Git source snapshot records `head: "not-a-git-repo"` and retains source byte pins. When mixed ranking settings are intentionally allowed within either arm, the report names each affected case and input, every varying setting, and its value, including unset versus empty values. The current R4 measurements and frozen execution instrument remain unchanged; these checks qualify retained inputs without rerunning inference.

The supported fingerprint schema is v4 with all 11 fields: `model`, `artifacts`, `dim`, `pooling`, `device`, `ort`, `pipeline`, `maxseq`, `norm`, `chunker`, and `chunk`. The comparator validates their domains and model/device agreement, including `gpu`/`cuda` aliases, and reconciles explicit model, provider, dimension, and pooling initialization fields in retained logs. A dynamic `api1.<minor>/dylib` runtime requires an `ort_runtime` byte pin; a static `api1.<minor>/static:<16-hex-build-id>` runtime carries its build identity and omits that pin. Both modes permit an `onnx_data` sidecar in the producer's artifact order. Every controlled record must declare a string query, and paired query text must match. Unsupported versions, incomplete fingerprints, contradictory runtime evidence, and missing/non-text queries are refused even with `--allow-mismatch`. Static-runtime and alternate-configuration tests use explicit fixtures and do not claim additional model inference.
