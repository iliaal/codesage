#!/usr/bin/env python3
"""Derive CodeSage's optimized ONNX graphs for jina-embeddings-v2-base-code.

Reads the upstream graph at a pinned revision (sha256-verified) and writes:

  onnx/model.onnx            fp32, masked mean pooling + L2 normalization
                             folded into the graph; output
                             `sentence_embedding` [batch, 768]. Portable to
                             every execution provider.
  onnx/model_cuda_fp16.onnx  the same pooling, attention fused into
                             com.microsoft MultiHeadAttention with ALiBi and
                             the key-padding mask passed as `attention_bias`,
                             weights in fp16 (inputs and output stay int64 /
                             fp32). For the CUDA execution provider.
  tokenizer.json             copied unchanged from upstream.

The upstream graph is a decomposed opset-11 export whose attention ONNX
Runtime's transformer optimizer does not fuse (ALiBi plus QK LayerNorm), so
every layer materializes [batch, 12, seq, seq] scores several times, and its
token-level output forces a [batch, seq, 768] device-to-host copy per batch.

Usage: scripts/derive-jina-onnx.py OUT_DIR
Requires: onnx, onnxruntime (for its float16 converter), huggingface_hub.
"""

import hashlib
import shutil
import sys
from pathlib import Path

import numpy as np
import onnx
from huggingface_hub import hf_hub_download
from onnx import TensorProto, helper, numpy_helper

UPSTREAM = "jinaai/jina-embeddings-v2-base-code"
REVISION = "516f4baf13dec4ddddda8631e019b5737c8bc250"
UPSTREAM_SHA256 = {
    "tokenizer.json": "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",  # gitleaks:allow (sha256 pin, not a credential)
    "onnx/model.onnx": "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
}
LAYERS = 12
HEADS = 12
HIDDEN = 768
# Finite so it survives fp16 (upstream fills with -FLT_MAX, which overflows).
MASK_FILL = -10000.0


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


def const(name, value):
    return helper.make_node(
        "Constant", [], [name], value=numpy_helper.from_array(np.array(value, np.float32))
    )


def fuse_attention(model):
    g = model.graph
    nodes = [
        helper.make_node("Cast", ["attention_mask"], ["mha_mask_f"], to=TensorProto.FLOAT),
        const("mha_one", 1.0),
        const("mha_fill", MASK_FILL),
        helper.make_node("Sub", ["mha_one", "mha_mask_f"], ["mha_pad"]),
        helper.make_node("Mul", ["mha_pad", "mha_fill"], ["mha_pad_bias"]),
        helper.make_node("Unsqueeze", ["mha_pad_bias"], ["mha_pad_bias4"], axes=[1, 2]),
        # /encoder/Cast_2 is the shared ALiBi tensor [1, heads, seq, seq].
        helper.make_node("Add", ["/encoder/Cast_2_output_0", "mha_pad_bias4"], ["mha_bias"]),
    ]
    for i in range(LAYERS):
        p = f"/encoder/layer.{i}/attention/self/"
        context = p + "Reshape_3_output_0"
        producers = [n for n in g.node if context in n.output]
        if len(producers) != 1:
            sys.exit(f"layer {i}: expected one producer of {context}, found {len(producers)}")
        g.node.remove(producers[0])
        nodes.append(
            helper.make_node(
                "MultiHeadAttention",
                [
                    p + "layer_norm_q/Add_1_output_0",
                    p + "layer_norm_k/Add_1_output_0",
                    p + "value/Add_output_0",
                    "",
                    "",
                    "mha_bias",
                ],
                [context],
                domain="com.microsoft",
                num_heads=HEADS,
                name=f"MultiHeadAttention_{i}",
            )
        )
    g.node.extend(nodes)
    model.opset_import.append(helper.make_opsetid("com.microsoft", 1))


def fold_pooling(model):
    g = model.graph
    hidden = g.output[0].name
    g.node.extend(
        [
            helper.make_node("Cast", ["attention_mask"], ["pool_mask"], to=TensorProto.FLOAT),
            helper.make_node("Unsqueeze", ["pool_mask"], ["pool_mask3"], axes=[2]),
            helper.make_node("Mul", [hidden, "pool_mask3"], ["pool_masked"]),
            helper.make_node("ReduceSum", ["pool_masked"], ["pool_sum"], axes=[1], keepdims=0),
            helper.make_node("ReduceSum", ["pool_mask3"], ["pool_count"], axes=[1], keepdims=0),
            helper.make_node("Div", ["pool_sum", "pool_count"], ["pool_mean"]),
            helper.make_node("LpNormalization", ["pool_mean"], ["sentence_embedding"], axis=1, p=2),
        ]
    )
    del g.output[:]
    g.output.append(
        helper.make_tensor_value_info("sentence_embedding", TensorProto.FLOAT, ["batch", HIDDEN])
    )


def prune_and_sort(model):
    g = model.graph
    while True:
        used = {o.name for o in g.output}
        for n in g.node:
            used.update(n.input)
        dead = [n for n in g.node if not any(o in used for o in n.output)]
        if not dead:
            break
        for n in dead:
            g.node.remove(n)
    ready = {i.name for i in g.input} | {t.name for t in g.initializer} | {""}
    pending, ordered = list(g.node), []
    while pending:
        rest = []
        for n in pending:
            if all(i in ready for i in n.input):
                ordered.append(n)
                ready.update(n.output)
            else:
                rest.append(n)
        if len(rest) == len(pending):
            sys.exit("graph has unresolvable inputs after rewriting")
        pending = rest
    del g.node[:]
    g.node.extend(ordered)
    onnx.checker.check_model(model)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    out = Path(sys.argv[1])
    (out / "onnx").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(fetch("tokenizer.json"), out / "tokenizer.json")
    source = fetch("onnx/model.onnx")

    pooled = onnx.load(source)
    fold_pooling(pooled)
    prune_and_sort(pooled)
    onnx.save(pooled, out / "onnx/model.onnx")

    from onnxruntime.transformers.float16 import convert_float_to_float16

    fused = onnx.load(source)
    fuse_attention(fused)
    fold_pooling(fused)
    prune_and_sort(fused)
    if any(n.op_type == "Softmax" for n in fused.graph.node):
        sys.exit("attention fusion left a Softmax behind")
    fused = convert_float_to_float16(fused, keep_io_types=True)
    onnx.save(fused, out / "onnx/model_cuda_fp16.onnx")

    for name in ("tokenizer.json", "onnx/model.onnx", "onnx/model_cuda_fp16.onnx"):
        print(f"{sha256(out / name)}  {name}")


if __name__ == "__main__":
    main()
