#!/usr/bin/env python3
"""Derive CodeSage's optimized ONNX graphs for jina-embeddings-v2-base-code.

Reads the upstream graph at a pinned revision (sha256-verified) and writes:

  onnx/model.onnx            fp32, token_type_embeddings[0] folded into
                             word_embeddings, masked mean pooling + L2
                             normalization folded into standard ONNX ops;
                             output `sentence_embedding` [batch, 768].
                             Portable to every execution provider.
  onnx/model_cuda_fp16.onnx  the same embedding fold, attention fused into
                             com.microsoft MultiHeadAttention with ALiBi and
                             the key-padding mask passed as `attention_bias`,
                             GEGLU fused into com.microsoft BiasSplitGelu,
                             24 post-norm residual MatMul biases folded into
                             com.microsoft SkipLayerNormalization, backbone
                             weights in fp16, and fp32 GPU-native masked mean
                             pooling + L2 normalization (no LpNormalization
                             CPU round-trip). For the CUDA execution provider.
  tokenizer.json             copied unchanged from upstream.

The upstream graph is a decomposed opset-11 export whose attention ONNX
Runtime's transformer optimizer does not fuse (ALiBi plus QK LayerNorm), so
every layer materializes [batch, 12, seq, seq] scores several times, and its
token-level output forces a [batch, seq, 768] device-to-host copy per batch.

Usage: scripts/derive-jina-onnx.py OUT_DIR
Requires: onnx, onnxruntime (for its float16 converter), huggingface_hub.
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

UPSTREAM = "jinaai/jina-embeddings-v2-base-code"
REVISION = "516f4baf13dec4ddddda8631e019b5737c8bc250"
UPSTREAM_SHA256 = {
    "tokenizer.json": "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",  # gitleaks:allow (sha256 pin, not a credential)
    "onnx/model.onnx": "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b",
}
LAYERS = 12
HEADS = 12
HIDDEN = 768
MLP_GATED_DIM = 6144
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


def fold_token_type_embeddings(model):
    """Fold constant zero token_type_embeddings[0] into word_embeddings.weight.

    Upstream creates an all-zero token_type_ids tensor on every forward pass,
    gathers row 0 of token_type_embeddings, and adds it to word_embeddings.
    """
    g = model.graph
    inits = {t.name: t for t in g.initializer}
    consts = {
        n.output[0]: numpy_helper.to_array(a.t)
        for n in g.node
        if n.op_type == "Constant"
        for a in n.attribute
        if a.name == "value"
    }
    zeros_table = consts.get("onnx::Slice_268")
    if zeros_table is None or not np.all(zeros_table == 0):
        sys.exit("expected all-zero token_type_ids table onnx::Slice_268 before folding")
    w_emb = numpy_helper.to_array(inits["embeddings.word_embeddings.weight"]).copy()
    tt_emb = numpy_helper.to_array(inits["embeddings.token_type_embeddings.weight"])
    w_emb += tt_emb[0:1, :]
    inits["embeddings.word_embeddings.weight"].CopyFrom(
        numpy_helper.from_array(w_emb, "embeddings.word_embeddings.weight")
    )
    rewired = 0
    for n in g.node:
        for idx, inp in enumerate(n.input):
            if inp == "/embeddings/Add_output_0":
                n.input[idx] = "/embeddings/word_embeddings/Gather_output_0"
                rewired += 1
    if rewired == 0:
        sys.exit("failed to rewire /embeddings/Add_output_0 consumers")


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
    if not any(o.domain == "com.microsoft" for o in model.opset_import):
        model.opset_import.append(helper.make_opsetid("com.microsoft", 1))


def fuse_bias_split_gelu(model):
    """Replace GEGLU (Slice(0:3072) * Gelu(Slice(3072:6144))) with BiasSplitGelu."""
    g = model.graph
    g.initializer.append(
        numpy_helper.from_array(np.zeros((MLP_GATED_DIM,), dtype=np.float32), "geglu_zero_bias")
    )
    for i in range(LAYERS):
        p = f"/encoder/layer.{i}/mlp/"
        up_out = p + "up_gated_layer/MatMul_output_0"
        mul_out = p + "Mul_output_0"
        producers = [n for n in g.node if mul_out in n.output]
        if len(producers) != 1:
            sys.exit(f"layer {i}: expected one producer of {mul_out}, found {len(producers)}")
        g.node.remove(producers[0])
        g.node.append(
            helper.make_node(
                "BiasSplitGelu",
                [up_out, "geglu_zero_bias"],
                [mul_out],
                domain="com.microsoft",
                name=f"BiasSplitGelu_{i}",
            )
        )


def _split_matmul_bias(node, inits, by_out, hidden):
    """Return (bias_initializer, matmul_output) if `node` is `Add(bias, MatMul)` in either order."""
    if not node or node.op_type != "Add" or len(node.input) != 2:
        return None
    for bi, mi in ((0, 1), (1, 0)):
        b_name, m_name = node.input[bi], node.input[mi]
        if b_name in inits and list(inits[b_name].dims) == [hidden]:
            m_node = by_out.get(m_name)
            if m_node and m_node.op_type == "MatMul":
                return b_name, m_name
    return None


def fuse_skip_layer_norm_biases(model):
    """Fold attention-output and MLP-down biases into SkipLayerNormalization."""
    g = model.graph
    by_out = {o: n for n in g.node for o in n.output}
    inits = {t.name: t for t in g.initializer}
    consts = {
        n.output[0]: numpy_helper.to_array(a.t)
        for n in g.node
        if n.op_type == "Constant"
        for a in n.attribute
        if a.name == "value"
    }
    for i in range(LAYERS):
        for target in (
            f"/encoder/layer.{i}/attention/output/LayerNorm/Add_1",
            f"/encoder/layer.{i}/layer_norm_2/Add_1",
        ):
            matches = [n for n in g.node if n.name == target]
            if len(matches) != 1:
                sys.exit(f"expected one node named {target}, found {len(matches)}")
            ln_add = matches[0]
            ln_mul = by_out[ln_add.input[0]]
            ln_div = by_out[ln_mul.input[0]]
            ln_sub = by_out[ln_div.input[0]]
            ln_sqrt = by_out[ln_div.input[1]]
            ln_eps_add = by_out[ln_sqrt.input[0]]
            eps_name = ln_eps_add.input[1] if ln_eps_add.input[1] in consts else ln_eps_add.input[0]
            eps = float(consts[eps_name].item())
            res_add = by_out[ln_sub.input[0]]
            a0, a1 = res_add.input[0], res_add.input[1]
            p0, p1 = by_out.get(a0), by_out.get(a1)
            bias_in, inp0, inp1 = None, a0, a1
            if found := _split_matmul_bias(p0, inits, by_out, HIDDEN):
                bias_in, inp0 = found
            elif found := _split_matmul_bias(p1, inits, by_out, HIDDEN):
                bias_in, inp1 = found
            if bias_in is None:
                sys.exit(f"could not locate {HIDDEN}-dim MatMul bias feeding {target}")
            g.node.remove(ln_add)
            g.node.append(
                helper.make_node(
                    "SkipLayerNormalization",
                    [inp0, inp1, ln_mul.input[1], ln_add.input[1], bias_in],
                    [ln_add.output[0]],
                    domain="com.microsoft",
                    epsilon=eps,
                    name=f"SkipLN_{target}",
                )
            )


def fold_pooling(model, cast_hidden_fp32=False):
    """Append fp32 masked mean pooling + L2 normalization using standard ops.

    Avoids `LpNormalization`, which lacks a CUDA fp16 kernel in ORT and forces
    a MemcpyToHost -> CPU LpNormalization -> MemcpyFromHost round-trip.
    """
    g = model.graph
    hidden = g.output[0].name
    g.initializer.append(numpy_helper.from_array(np.array([1e-12], dtype=np.float32), "pool_eps"))
    h_in = hidden
    if cast_hidden_fp32:
        g.node.append(helper.make_node("Cast", [hidden], ["pool_hidden_f32"], to=TensorProto.FLOAT))
        h_in = "pool_hidden_f32"
    g.node.extend(
        [
            helper.make_node("Cast", ["attention_mask"], ["pool_mask"], to=TensorProto.FLOAT),
            helper.make_node("Unsqueeze", ["pool_mask"], ["pool_mask3"], axes=[2]),
            helper.make_node("Mul", [h_in, "pool_mask3"], ["pool_masked"]),
            helper.make_node("ReduceSum", ["pool_masked"], ["pool_sum"], axes=[1], keepdims=0),
            helper.make_node("ReduceSum", ["pool_mask3"], ["pool_count"], axes=[1], keepdims=0),
            helper.make_node("Max", ["pool_count", "pool_eps"], ["pool_count_safe"]),
            helper.make_node("Div", ["pool_sum", "pool_count_safe"], ["pool_mean"]),
            helper.make_node("Mul", ["pool_mean", "pool_mean"], ["pool_sq"]),
            helper.make_node("ReduceSum", ["pool_sq"], ["pool_ss"], axes=[1], keepdims=1),
            helper.make_node("Sqrt", ["pool_ss"], ["pool_norm"]),
            helper.make_node("Max", ["pool_norm", "pool_eps"], ["pool_norm_safe"]),
            helper.make_node("Div", ["pool_mean", "pool_norm_safe"], ["sentence_embedding"]),
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
    used_inits = {inp for n in g.node for inp in n.input}
    keep_inits = [t for t in g.initializer if t.name in used_inits]
    del g.initializer[:]
    g.initializer.extend(keep_inits)

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
    fold_token_type_embeddings(pooled)
    fold_pooling(pooled, cast_hidden_fp32=False)
    prune_and_sort(pooled)
    onnx.save(pooled, out / "onnx/model.onnx")

    from onnxruntime.transformers.float16 import convert_float_to_float16

    fused = onnx.load(source)
    fold_token_type_embeddings(fused)
    fuse_attention(fused)
    fuse_bias_split_gelu(fused)
    fuse_skip_layer_norm_biases(fused)
    prune_and_sort(fused)
    if any(n.op_type == "Softmax" for n in fused.graph.node):
        sys.exit("attention fusion left a Softmax behind")
    if any(n.op_type == "Erf" for n in fused.graph.node):
        sys.exit("GEGLU fusion left an Erf behind")
    fused = convert_float_to_float16(fused, keep_io_types=False)
    fold_pooling(fused, cast_hidden_fp32=True)
    prune_and_sort(fused)
    onnx.save(fused, out / "onnx/model_cuda_fp16.onnx")

    for name in ("tokenizer.json", "onnx/model.onnx", "onnx/model_cuda_fp16.onnx"):
        print(f"{sha256(out / name)}  {name}")


if __name__ == "__main__":
    main()
