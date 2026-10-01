#!/usr/bin/env python3
"""Derive CodeSage's CUDA graph for cross-encoder/ms-marco-MiniLM-L6-v2.

Reads the upstream graph at a pinned revision (sha256-verified) and writes:

  onnx/model.onnx            copied unchanged from upstream; portable to every
                             execution provider.
  onnx/model_cuda_fp16.onnx  ONNX Runtime's BERT transformer optimizer
                             (EmbedLayerNormalization, Attention,
                             SkipLayerNormalization, BiasGelu fusions), weights
                             in fp16 with int64 inputs and fp32 logits. For the
                             CUDA execution provider.
  tokenizer.json             copied unchanged from upstream.

Usage: scripts/derive-reranker-onnx.py OUT_DIR
Requires: onnx, onnxruntime (for its transformer optimizer), huggingface_hub.
"""

import hashlib
import shutil
import sys
from pathlib import Path

from huggingface_hub import hf_hub_download

UPSTREAM = "cross-encoder/ms-marco-MiniLM-L6-v2"
REVISION = "c5ee24cb16019beea0893ab7796b1df96625c6b8"
UPSTREAM_SHA256 = {
    "tokenizer.json": "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",  # gitleaks:allow (sha256 pin, not a credential)
    "onnx/model.onnx": "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a",
}
HEADS = 12
HIDDEN = 384
LAYERS = 6


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fetch(name):
    path = hf_hub_download(UPSTREAM, name, revision=REVISION)
    got = sha256(path)
    if got != UPSTREAM_SHA256[name]:
        sys.exit(f"{name}: sha256 {got} does not match the pinned {UPSTREAM_SHA256[name]}")
    return path


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    (out / "onnx").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(fetch("tokenizer.json"), out / "tokenizer.json")
    source = fetch("onnx/model.onnx")
    shutil.copyfile(source, out / "onnx/model.onnx")

    from onnxruntime.transformers.optimizer import optimize_model

    fused = optimize_model(
        source, model_type="bert", num_heads=HEADS, hidden_size=HIDDEN, opt_level=0, use_gpu=True
    )
    stats = fused.get_fused_operator_statistics()
    if stats.get("Attention") != LAYERS or stats.get("EmbedLayerNormalization") != 1:
        sys.exit(f"incomplete fusion: {stats}")
    fused.convert_float_to_float16(keep_io_types=True)
    fused.save_model_to_file(str(out / "onnx/model_cuda_fp16.onnx"))

    for name in ("tokenizer.json", "onnx/model.onnx", "onnx/model_cuda_fp16.onnx"):
        print(f"{sha256(out / name)}  {name}")


if __name__ == "__main__":
    main()
