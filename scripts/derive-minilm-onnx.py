#!/usr/bin/env python3
"""Derive CodeSage's optimized ONNX graphs for all-MiniLM-L6-v2.

Reads the upstream graph at a pinned revision (sha256-verified) and writes:

  onnx/model.onnx            fp32, masked mean pooling + L2 normalization
                             folded into standard opset-14 ONNX ops; output
                             `sentence_embedding` [batch_size, 384]. Portable
                             to every execution provider (CPU, CoreML, CUDA).
  onnx/model_cuda_fp16.onnx  ONNX Runtime's BERT transformer optimizer
                             (EmbedLayerNormalization, Attention,
                             SkipLayerNormalization, BiasGelu fusions),
                             backbone weights in fp16, and fp32 GPU-native
                             masked mean pooling + L2 normalization; output
                             `sentence_embedding` [batch_size, 384]. For the
                             CUDA execution provider.
  tokenizer.json             copied unchanged from upstream.

Usage: scripts/derive-minilm-onnx.py OUT_DIR
Requires: onnx, onnxruntime (for its transformer optimizer), huggingface_hub.
Pinned hashes were generated with onnx==1.21.0 and onnxruntime==1.24.4.
"""

import hashlib
import shutil
import sys
from pathlib import Path

import numpy as np
import onnx
from huggingface_hub import hf_hub_download
from onnx import TensorProto, helper, numpy_helper

UPSTREAM = "sentence-transformers/all-MiniLM-L6-v2"
REVISION = "c9745ed1d9f207416be6d2e6f8de32d1f16199bf"
UPSTREAM_SHA256 = {
    "tokenizer.json": "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",  # gitleaks:allow (sha256 pin, not a credential)
    "onnx/model.onnx": "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452",
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


def fold_pooling_opset14(model, cast_hidden_fp32=False):
    """Append opset-14 fp32 masked mean pooling + L2 normalization.

    Avoids `LpNormalization`, which lacks a CUDA fp16 kernel in ORT, and uses
    standard opset-14 input-axes `Unsqueeze` and `ReduceSum`.
    """
    g = model.graph
    hidden = g.output[0].name
    g.initializer.extend(
        [
            numpy_helper.from_array(np.array([2], dtype=np.int64), name="pool_unsq_axes"),
            numpy_helper.from_array(np.array([1], dtype=np.int64), name="pool_red_axes"),
            numpy_helper.from_array(np.array([1e-12], dtype=np.float32), name="pool_eps"),
        ]
    )
    h_in = hidden
    if cast_hidden_fp32:
        g.node.append(helper.make_node("Cast", [hidden], ["pool_hidden_f32"], to=TensorProto.FLOAT))
        h_in = "pool_hidden_f32"
    g.node.extend(
        [
            helper.make_node("Cast", ["attention_mask"], ["pool_mask"], to=TensorProto.FLOAT),
            helper.make_node("Unsqueeze", ["pool_mask", "pool_unsq_axes"], ["pool_mask3"]),
            helper.make_node("Mul", [h_in, "pool_mask3"], ["pool_masked"]),
            helper.make_node("ReduceSum", ["pool_masked", "pool_red_axes"], ["pool_sum"], keepdims=0),
            helper.make_node("ReduceSum", ["pool_mask3", "pool_red_axes"], ["pool_count"], keepdims=0),
            helper.make_node("Max", ["pool_count", "pool_eps"], ["pool_count_safe"]),
            helper.make_node("Div", ["pool_sum", "pool_count_safe"], ["pool_mean"]),
            helper.make_node("Mul", ["pool_mean", "pool_mean"], ["pool_sq"]),
            helper.make_node("ReduceSum", ["pool_sq", "pool_red_axes"], ["pool_ss"], keepdims=1),
            helper.make_node("Sqrt", ["pool_ss"], ["pool_norm"]),
            helper.make_node("Max", ["pool_norm", "pool_eps"], ["pool_norm_safe"]),
            helper.make_node("Div", ["pool_mean", "pool_norm_safe"], ["sentence_embedding"]),
        ]
    )
    del g.output[:]
    g.output.append(
        helper.make_tensor_value_info("sentence_embedding", TensorProto.FLOAT, ["batch_size", HIDDEN])
    )


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    (out / "onnx").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(fetch("tokenizer.json"), out / "tokenizer.json")
    source = fetch("onnx/model.onnx")

    portable = onnx.load(source)
    fold_pooling_opset14(portable, cast_hidden_fp32=False)
    onnx.checker.check_model(portable)
    onnx.save(portable, out / "onnx/model.onnx")

    from onnxruntime.transformers.optimizer import optimize_model

    fused = optimize_model(
        source, model_type="bert", num_heads=HEADS, hidden_size=HIDDEN, opt_level=0, use_gpu=True
    )
    stats = fused.get_fused_operator_statistics()
    if (
        stats.get("Attention") != LAYERS
        or stats.get("EmbedLayerNormalization") != 1
        or stats.get("SkipLayerNormalization") != LAYERS * 2
        or stats.get("BiasGelu") != LAYERS
    ):
        sys.exit(f"incomplete fusion: {stats}")
    fused.convert_float_to_float16(keep_io_types=False)
    fold_pooling_opset14(fused.model, cast_hidden_fp32=True)
    cuda_out = out / "onnx/model_cuda_fp16.onnx"
    fused.save_model_to_file(str(cuda_out))
    onnx.checker.check_model(str(cuda_out))

    for name in ("tokenizer.json", "onnx/model.onnx", "onnx/model_cuda_fp16.onnx"):
        print(f"{sha256(out / name)}  {name}")


if __name__ == "__main__":
    main()
