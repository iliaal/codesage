"""Resolve cached reranker artifacts from the repository's canonical loader pins."""
from __future__ import annotations

import os
import re
import tomllib
from pathlib import Path

from _retrieval_controls import InvalidRun, file_pin


def model_pins(source: str) -> list[dict]:
    tables = re.findall(r"const MODEL_PINS: &\[ModelPin\] = &\[(.*?)\n\];", source, re.S)
    if len(tables) != 1:
        raise InvalidRun("cannot identify canonical MODEL_PINS; pass --reranker-artifact explicitly")
    table = re.sub(r"(?m)^\s*//[^\n]*", "", tables[0])
    pattern = re.compile(
        r'ModelPin\s*\{\s*model:\s*"(?P<model>[^"\n]+)",\s*'
        r'repo:\s*"(?P<repo>[^"\n]+)",\s*'
        r'revision:\s*(?P<revision>"[0-9a-f]{40}"|[A-Z][A-Z0-9_]*),\s*'
        r'tokenizer_sha256:\s*"(?P<tokenizer>[0-9a-f]{64})",\s*'
        r'onnx_sha256:\s*"(?P<onnx>[0-9a-f]{64})",\s*'
        r'onnx_data_sha256:\s*(?:None|Some\("(?P<sidecar>[0-9a-f]{64})"\)),\s*'
        r'cuda_onnx:\s*(?:None|Some\(GraphPin\s*\{\s*'
        r'file:\s*"(?P<cuda_file>[^"\n]+)",\s*'
        r'sha256:\s*"(?P<cuda_hash>[0-9a-f]{64})",\s*\}\)),\s*'
        r'max_seq_length:\s*[A-Za-z0-9_]+,\s*\},\s*'
    )
    pins = []
    remaining = table.strip()
    while remaining:
        match = pattern.match(remaining)
        if not match:
            raise InvalidRun("unsupported canonical ModelPin syntax; pass --reranker-artifact explicitly")
        pin = match.groupdict()
        revision = pin["revision"]
        if revision.startswith('"'):
            pin["revision"] = revision.strip('"')
        else:
            values = re.findall(rf'(?m)^const {re.escape(revision)}: &str = "([0-9a-f]{{40}})";', source)
            if len(values) != 1:
                raise InvalidRun(f"missing or ambiguous canonical revision {revision}")
            pin["revision"] = values[0]
        pins.append(pin)
        remaining = remaining[match.end():].lstrip()
    if not pins or len({pin["model"] for pin in pins}) != len(pins):
        raise InvalidRun("empty or ambiguous canonical model pins")
    return pins


def cached_reranker_artifacts(project: Path, pin_source: Path | None = None) -> list[Path]:
    config = tomllib.loads((project / ".codesage/config.toml").read_text())
    embedding = config.get("embedding", {})
    model = embedding.get("reranker")
    if not model:
        return []
    if os.environ.get("CODESAGE_ALLOW_ANY_MODEL") in ("1", "true"):
        raise InvalidRun("automatic reranker pins require CODESAGE_ALLOW_ANY_MODEL unset; "
                         "pass --reranker-artifact explicitly for an unpinned load")
    source = pin_source or Path(__file__).resolve().parents[1] / "crates/embed/src/model.rs"
    pins = [pin for pin in model_pins(source.read_text()) if pin["model"] == model]
    if len(pins) != 1:
        raise InvalidRun(f"no canonical reranker pin for {model!r}; pass --reranker-artifact explicitly")
    pin = pins[0]
    hf_home = os.environ.get("HF_HOME")
    if hf_home is None or any(0xD800 <= ord(c) <= 0xDFFF for c in hf_home):
        home = os.environ.get("HOME")
        if not home:
            raise InvalidRun("cannot resolve reranker cache: set HF_HOME or HOME")
        hf_home = str(Path(home) / ".cache/huggingface")
    cache_root = Path(hf_home)
    if not cache_root.is_absolute():
        cache_root = project / cache_root
    repository = cache_root / "hub" / ("models--" + pin["repo"].replace("/", "--"))
    reference = repository / "refs" / pin["revision"]
    commit = reference.read_text()
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise InvalidRun(f"invalid reranker cache revision pointer: {reference}")
    snapshot = repository / "snapshots" / commit
    graph, graph_hash = "onnx/model.onnx", pin["onnx"]
    if embedding.get("device", "cpu").strip().lower() in ("gpu", "cuda") and pin["cuda_file"]:
        graph, graph_hash = pin["cuda_file"], pin["cuda_hash"]
    artifacts = [("tokenizer.json", pin["tokenizer"]), (graph, graph_hash)]
    if pin["sidecar"]:
        artifacts.append(("onnx/model.onnx_data", pin["sidecar"]))
    elif (snapshot / graph).with_name("model.onnx_data").exists():
        raise InvalidRun("undeclared reranker ONNX sidecar exists; automatic pinning refused")
    paths = []
    for name, expected in artifacts:
        if Path(name).is_absolute() or ".." in Path(name).parts:
            raise InvalidRun(f"invalid canonical reranker artifact path: {name!r}")
        path = snapshot / name
        if file_pin(path)["sha256"] != expected:
            raise InvalidRun(f"cached reranker artifact hash differs from canonical pin: {path}")
        paths.append(path.resolve(strict=True))
    return paths
