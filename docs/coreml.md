# CoreML acceleration on macOS

CodeSage can use Apple Silicon's GPU and Neural Engine for ONNX embedding inference through CoreML.

## Build

```bash
cargo build --features coreml
```

The `coreml` feature statically links ONNX Runtime with the CoreML execution provider. Without this feature, `device = "coreml"` in config will error at session init.

## Configure

Set `device = "coreml"` in `.codesage/config.toml`:

```toml
[embedding]
model = "jinaai/jina-embeddings-v2-base-code"
device = "coreml"
```

## How it works

At session creation, ONNX Runtime partitions the model graph: supported ops (matmul, softmax, layer norm, etc.) go to CoreML submodels, unsupported ops (shape ops, reshape) stay on CPU.

- CoreML compilation writes `.mlmodel` files to `/tmp/` once per process
- The CoreML EP typically offloads ~80% of nodes (`796 / 1012` for Jina embeddings)
- Remaining CPU ops use ORT's BFCArena, which grows to ~1.5 GB peak with default settings
- `ComputeUnits::All` is used (GPU + ANE); currently **not configurable** without a code change

## Batch size

The Jina v2 base-code model (611 MB, 32-layer 1024-dim transformer) uses significant intermediate memory. The default `BATCH_SIZE` is **8** (`crates/embed/src/config.rs`), tuned for 48 GB RAM. To adjust:

```rust
pub const BATCH_SIZE: usize = 8;  // memory ~ batch * 180 MB
```

Increase for faster throughput if you have headroom; decrease if memory pressure is tight. Each batch unit costs ~180 MB of CPU arena memory at peak for this model.

## Model cache

ONNX model files are cached at `~/.cache/huggingface/hub/models--<org>--<model>/` on first fetch. CoreML compiled artifacts live in `/tmp/onnxruntime-*.model.mlmodel` and are ephemeral (recompiled each run).

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `Binary built without coreml feature` | Rebuild with `--features coreml` |
| `Failed to register CoreMLExecutionProvider` | Check ORT compatibility with the model opset |
| Process killed (signal 9) during embedding | OOM — reduce `BATCH_SIZE` in `config.rs` |
| `Context leak detected, CoreAnalytics returned false` | CoreML internal diagnostic only; can be ignored |

## Cargo dependency

The relevant `Cargo.toml` snippet that enables CoreML:

```toml
ort = { version = "2.0.0-rc.12", features = ["coreml"] }
```

## Verified working (2026-06-10)

The following output was captured from a successful run on a 14-core Apple Silicon Mac with 48 GB RAM.

### CoreML registration

```
Successfully registered CoreMLExecutionProvider
CoreML partitions: 796 / 1012 nodes (97 submodels)
```

### Index

```
RUST_LOG=codesage=info cargo run --features coreml -- index --verbose

structural index complete  files_indexed=1  files_skipped=174  symbols=17  refs=18
feature mapping complete  created=0  updated=29  removed=0  total=29
semantic index complete   files_processed=126  chunks=1160
```

~7 minutes for 126 files (115 s for batch 1 incl. CoreML compilation, ~110 s for batch 2, ~66 s for batch 3).

### Status

```
Project root: /Users/spolyakov/codesage
Database:     /Users/spolyakov/codesage/.codesage/index.db
Files:        175
Symbols:      4197
References:   14990
Chunks:       2229
Drift:        fresh (HEAD ...)
Semantic:     fresh for model jinaai/jina-embeddings-v2-base-code (175 files)
```

### Search

```json
codesage search --json "config batch size embedding" | jq '.[0].file_path'
"crates/embed/src/model.rs"
"crates/embed/src/config.rs"
"crates/graph/src/semantic.rs"
"crates/storage/src/db/mod.rs"
```

Top result score: `0.945` (cross-encoder reranked). Semantic + rerank pipeline functional end-to-end.
