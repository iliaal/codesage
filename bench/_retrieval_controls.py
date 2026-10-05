"""Executable retrieval controls and byte/provenance contracts."""
from __future__ import annotations

import hashlib
import json
import math
import os
import random
import re
import shutil
import sqlite3
import subprocess
import tomllib
from pathlib import Path

CONTROL_PROTOCOL = "retrieval-controls-v2"
QUERY_LOG_FILTER = "warn,codesage=info"
QUERY_NO_COLOR = "1"
QUERY_RUNTIME = {"mode": "private-per-query-v1", "daemon_reuse": False,
                 "directory_policy": "fresh-empty-0700", "log_filter": QUERY_LOG_FILTER,
                 "no_color": QUERY_NO_COLOR}
ARMS = ("codesage", "placebo", "rg", "rg_matched")
GIT_REPOSITORY_SELECTORS = {
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_NAMESPACE",
    "GIT_PREFIX", "GIT_SHALLOW_FILE", "GIT_GRAFT_FILE", "GIT_IMPLICIT_WORK_TREE",
    "GIT_REPLACE_REF_BASE", "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_REFERENCE_BACKEND",
}


class InvalidRun(ValueError):
    pass


def digest_json(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=True).encode()).hexdigest()


def file_pin(path: Path) -> dict:
    path = path.resolve(strict=True)
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    stat = path.stat()
    return {"path": str(path), "sha256": digest, "size": stat.st_size}


def resolve_executable(value: str) -> str:
    selected = shutil.which(value)
    if not selected:
        raise InvalidRun(f"executable not found or not executable: {value}")
    return str(Path(selected).resolve(strict=True))


def absolute_executable(value: str) -> str:
    return value if Path(value).is_absolute() else resolve_executable(value)


def git_head(project: Path) -> str:
    result = subprocess.run(["git", "rev-parse", "HEAD"], cwd=project, env=git_environment(),
                            capture_output=True, text=True, timeout=30)
    return result.stdout.strip() if result.returncode == 0 else "not-a-git-repo"


def git_environment() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if k not in GIT_REPOSITORY_SELECTORS}


def instrument_environment() -> dict[str, str]:
    return {k: v for k, v in sorted(os.environ.items())
            if ((k.startswith("CODESAGE_")
                 and k not in ("CODESAGE_BENCH_CORPUS_DIR", "CODESAGE_DAEMON_RUNTIME_DIR"))
                or k in ("ORT_DYLIB_PATH", "HF_HOME", "HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"))}


def validate_runtime(runtime: dict) -> None:
    if (not isinstance(runtime, dict) or any(runtime.get(k) != v for k, v in QUERY_RUNTIME.items())
            or not isinstance(runtime.get("pid"), int) or isinstance(runtime["pid"], bool)
            or runtime["pid"] <= 0 or runtime.get("directory_mode") != 0o700
            or runtime.get("returncode") != 0
            or runtime.get("entries_before") != [] or runtime.get("entries_after") != []
            or not isinstance(runtime.get("runtime_dir"), str)
            or not Path(runtime["runtime_dir"]).is_absolute()
            or not isinstance(runtime.get("reranker"), str) or not runtime["reranker"]
            or not isinstance(runtime.get("stderr"), str)):
        raise InvalidRun("missing or invalid private query-runtime evidence")
    stderr = runtime["stderr"]
    if runtime.get("stderr_sha256") != hashlib.sha256(stderr.encode()).hexdigest():
        raise InvalidRun("query-runtime stderr hash differs")
    if ("through the running daemon" in stderr or "daemon cannot embed" in stderr
            or "daemon reranking" in stderr):
        raise InvalidRun("unexpected daemon use in private query runtime")
    if "embedding model loaded" not in stderr:
        raise InvalidRun("private embedding initialization was not observed")
    if runtime.get("reranker") != "none" and not all(
            marker in stderr for marker in ("reranking privately", "reranker loaded")):
        raise InvalidRun("private reranker initialization was not observed")


def json_output(raw: str):
    start = next((i for i, ch in enumerate(raw) if ch in "[{"), None)
    if start is None:
        raise InvalidRun("missing JSON output")
    try:
        return json.loads(raw[start:])
    except json.JSONDecodeError as exc:
        raise InvalidRun("malformed JSON output") from exc


def search_rows(raw: str) -> list[tuple[str, str]]:
    data = json_output(raw)
    if isinstance(data, dict):
        data = data.get("results")
    if not isinstance(data, list):
        raise InvalidRun("search output needs a results list")
    rows = []
    for row in data:
        if (not isinstance(row, dict) or not isinstance(row.get("file_path"), str)
                or not row["file_path"] or not isinstance(row.get("content"), str)):
            raise InvalidRun("search row needs nonempty file_path and string content")
        rows.append((row["file_path"], row["content"]))
    return rows


def page(rows: list[tuple[str, str]], budget: int | None = None) -> tuple[list[tuple[str, str]], dict]:
    """Charge path LF content LF; count a file only after its full path header fits."""
    used = 0
    emitted = []
    truncated = False
    digest = hashlib.sha256()
    for path, content in rows:
        header = (path + "\n").encode("utf-8")
        available = len(header) if budget is None else budget - used
        if available < len(header):
            digest.update(header[:max(0, available)])
            used += max(0, available)
            truncated = True
            break
        body = (content + "\n").encode("utf-8")
        count = len(body) if budget is None else min(len(body), available - len(header))
        consumed = body[:count]
        digest.update(header)
        digest.update(consumed)
        emitted.append((path, consumed.decode("utf-8", errors="replace")))
        used += len(header) + count
        if count != len(body):
            truncated = True
            break
    return emitted, {"bytes": used, "budget_bytes": budget, "truncated": truncated,
                     "page_sha256": digest.hexdigest()}


def distinct(rows: list[tuple[str, str]], defining_file: str | None = None) -> list[tuple[str, str]]:
    seen = {defining_file}
    ordered = []
    for path, content in rows:
        if path not in seen:
            seen.add(path)
            ordered.append((path, content))
    return ordered


def placebo_rows(chunks: dict[str, list[str]], case_id: str, seed: int,
                  budget: int | None = None) -> list[tuple[str, str]]:
    rng = random.Random(hashlib.sha256(f"{seed}\0{case_id}".encode()).digest())
    paths = sorted(chunks)
    if not paths or any(not contents for contents in chunks.values()):
        raise InvalidRun("placebo needs a nonempty file/chunk universe")
    rows = []
    used = 0
    while not rows or (budget is not None and used < budget):
        rng.shuffle(paths)
        for path in paths:
            content = rng.choice(chunks[path])
            rows.append((path, content))
            used += len((path + "\n" + content + "\n").encode())
        if budget is None:
            break
    return rows


def run_rg(project: Path, paths: list[str], query: str, rg_bin: str) -> tuple[list[tuple[str, str]], dict]:
    rg_bin = absolute_executable(rg_bin)
    terms = list(dict.fromkeys(re.findall(r"[A-Za-z0-9_]+", query)))
    if not terms:
        raise InvalidRun("rg query has no literal tokens")
    cmd = [rg_bin, "--no-config", "--json", "--sort", "path", "--fixed-strings", "--ignore-case",
           "--no-ignore", "--hidden"]
    for term in terms:
        cmd += ["-e", term]
    paths = sorted(paths)
    commands = [cmd + ["--", *paths[i:i + 256]] for i in range(0, len(paths), 256)]
    stdout = []
    stderr = []
    codes = []
    for command in commands:
        try:
            result = subprocess.run(command, cwd=project, capture_output=True, text=True, timeout=120)
        except (OSError, subprocess.SubprocessError) as exc:
            raise InvalidRun(f"rg failed: {exc}") from exc
        if result.returncode not in (0, 1):
            raise InvalidRun(f"rg failed rc={result.returncode}: {result.stderr}")
        stdout.append(result.stdout)
        stderr.append(result.stderr)
        codes.append(result.returncode)
    output = "".join(stdout)
    rows = []
    try:
        for line in output.splitlines():
            event = json.loads(line)
            if event.get("type") == "match":
                data = event["data"]
                rows.append((data["path"]["text"].removeprefix("./"), data["lines"]["text"]))
    except (KeyError, TypeError, json.JSONDecodeError) as exc:
        raise InvalidRun("rg emitted invalid JSON or non-UTF8 paths/content") from exc
    return rows, {"command": commands[0] if len(commands) == 1 else cmd,
                  "commands": commands, "returncode": 0 if 0 in codes else 1,
                  "stdout_sha256": hashlib.sha256(output.encode()).hexdigest(),
                  "stderr": "".join(stderr), "terms": terms}


def capture_context(project: Path, codesage_bin: str, rg_bin: str,
                    reranker_artifacts: list[Path]) -> tuple[dict, dict[str, list[str]]]:
    codesage_bin = absolute_executable(codesage_bin)
    rg_bin = absolute_executable(rg_bin)
    config_path = project / ".codesage/config.toml"
    config = tomllib.loads(config_path.read_text())
    embedding = config.get("embedding", {})
    model = embedding.get("model", "jinaai/jina-embeddings-v2-base-code")
    reranker = embedding.get("reranker")
    if reranker and len(reranker_artifacts) < 2:
        raise InvalidRun("configured reranker needs --reranker-artifact for its tokenizer and selected ONNX graph")
    status = subprocess.run([codesage_bin, "status", "--json"], cwd=project,
                            capture_output=True, text=True, timeout=30)
    if status.returncode != 0:
        raise InvalidRun(f"codesage status failed: {status.stderr}")
    measured = json_output(status.stdout)
    if measured.get("semantic", {}).get("state") != "fresh":
        raise InvalidRun(f"semantic index is not fresh: {measured.get('semantic')}")
    if measured.get("interpretation", {}).get("stale_files", 0):
        raise InvalidRun("structural interpretations are stale; reindex the corpus explicitly")
    db = project / ".codesage/index.db"
    with sqlite3.connect(db.as_uri() + "?mode=ro", uri=True) as conn:
        conn.execute("BEGIN")
        record = conn.execute(
            "SELECT chunk_table, fingerprint, artifact_digest, artifact_stat_key FROM semantic_models WHERE model=?",
            (model,),
        ).fetchone()
        if not record or not all(record[1:]):
            raise InvalidRun("index lacks actual semantic fingerprint/artifact attestation")
        table, fingerprint, artifact_digest, stat_key = record
        fts = '"' + (table + "_fts").replace('"', '""') + '"'
        indexed = list(conn.execute("SELECT path, content_hash FROM semantic_files WHERE chunk_table=? ORDER BY path", (table,)))
        rows = list(conn.execute(f"SELECT rowid, file_path, content, start_line, end_line FROM {fts} ORDER BY rowid"))
    chunks: dict[str, list[str]] = {}
    indexed_hashes = dict(indexed)
    for _, path, content, _, _ in rows:
        if path not in indexed_hashes:
            raise InvalidRun(f"chunk file {path!r} lacks semantic freshness evidence")
        chunks.setdefault(path, []).append(content)
    if not chunks:
        raise InvalidRun("eligible semantic file universe is empty")
    sources = []
    for path in sorted(chunks):
        source = project / path
        if (Path(path).is_absolute() or ".." in Path(path).parts or source.is_symlink()
                or not source.resolve().is_relative_to(project)):
            raise InvalidRun(f"invalid indexed path {path!r}")
        data = source.read_bytes()
        if hashlib.sha256(data).hexdigest() != indexed_hashes[path]:
            raise InvalidRun(f"source differs from indexed semantic hash: {path}")
        sources.append({"path": path, "sha256": hashlib.sha256(data).hexdigest()})
    artifacts = []
    artifact_hash = hashlib.sha256()
    for field in stat_key.split(";"):
        label, value = field.split("=", 1)
        path, size, mtime = value.rsplit(":", 2)
        pin = file_pin(Path(path))
        if pin["size"] != int(size) or Path(path).stat().st_mtime_ns != int(mtime.replace(".", "")):
            raise InvalidRun(f"index artifact stat changed: {label}")
        artifacts.append({"label": label, **pin})
        artifact_hash.update(f"{label}={pin['sha256']}\n".encode())
    if artifact_hash.hexdigest() != artifact_digest:
        raise InvalidRun("index artifact bytes differ from persisted digest")
    context = {
        "head": git_head(project), "model": model, "reranker": reranker or "none",
        "device": embedding.get("device", "cpu"),
        "semantic_fingerprint": fingerprint, "artifact_digest": artifact_digest,
        "embedding_artifacts": artifacts,
        "reranker_artifacts": [file_pin(p) for p in reranker_artifacts],
        "config_sha256": file_pin(config_path)["sha256"],
        "environment": instrument_environment(), "query_runtime": QUERY_RUNTIME.copy(),
        "binary": file_pin(Path(codesage_bin)), "rg_binary": file_pin(Path(rg_bin)),
        "source_manifest": sources, "source_sha256": digest_json(sources),
        "eligible_files": sorted(chunks), "eligible_files_sha256": digest_json(sorted(chunks)),
        "semantic_files_sha256": digest_json(indexed), "chunks_sha256": digest_json(rows),
        "measurement": "retrieval; private inference per query, no daemon reuse; indexing pass mode is not measured",
        "score_validation": "file-ranking metrics recomputed from hits/gold; token costs require content not retained in this record",
    }
    return context, chunks


def validate_record(record: dict) -> None:
    arms = record.get("arms")
    if not isinstance(arms, dict) or set(arms) != set(ARMS):
        raise InvalidRun(f"case {record.get('id')!r}: missing or unexpected control arm")
    real = arms["codesage"]
    expected = record.get("expected_files")
    if not isinstance(expected, list) or not expected or any(not isinstance(p, str) or not p for p in expected):
        raise InvalidRun(f"case {record.get('id')!r}: invalid gold files")
    expected = set(expected)
    for name, arm in arms.items():
        if (not isinstance(arm, dict) or not isinstance(arm.get("hits"), list)
                or any(not isinstance(p, str) or not p for p in arm["hits"])
                or len(arm["hits"]) != len(set(arm["hits"]))
                or not isinstance(arm.get("bytes"), int) or isinstance(arm["bytes"], bool)
                or arm["bytes"] < 0):
            raise InvalidRun(f"case {record.get('id')!r}: invalid {name} arm")
        if arm.get("error"):
            raise InvalidRun(f"case {record.get('id')!r}: failed {name} arm: {arm['error']}")
        first = next((i for i, p in enumerate(arm["hits"], 1) if p in expected), None)
        if ("first_hit_rank" not in arm or isinstance(arm.get("first_hit_rank"), bool)
                or arm.get("first_hit_rank") != first):
            raise InvalidRun(f"case {record.get('id')!r}: {name} first-hit score disagrees with hits")
        recall_keys = {"recall@5", "recall@10"} | {key for key in arm if key.startswith("recall@")}
        for key in recall_keys:
            if not re.fullmatch(r"recall@[1-9][0-9]*", key):
                raise InvalidRun(f"case {record.get('id')!r}: invalid recall metric {key}")
            k = int(key.split("@")[1])
            recall = len(set(arm["hits"][:k]) & expected) / len(expected)
            if isinstance(arm.get(key), bool) or not isinstance(arm.get(key), (int, float)) or arm.get(key) != recall:
                raise InvalidRun(f"case {record.get('id')!r}: {name} recall@{k} disagrees with hits")
        dcg = sum(1 / math.log2(i + 2) for i, p in enumerate(arm["hits"][:10]) if p in expected)
        ideal = sum(1 / math.log2(i + 2) for i in range(min(10, len(expected))))
        derived = {"noise_before_first_hit": first - 1 if first else len(arm["hits"]),
                   "returned_count": len(arm["hits"]), "mrr": 1 / first if first else 0.0,
                   "ndcg@10": dcg / ideal}
        for metric, value in derived.items():
            stored = arm.get(metric)
            if isinstance(stored, bool) or not isinstance(stored, (int, float)) or stored != value:
                raise InvalidRun(f"case {record.get('id')!r}: {name} {metric} disagrees with hits")
    validate_runtime(real.get("runtime"))
    for name in ("placebo", "rg_matched"):
        if arms[name].get("budget_bytes") != real["bytes"] or arms[name]["bytes"] > real["bytes"]:
            raise InvalidRun(f"case {record.get('id')!r}: {name} byte budget differs")
    if arms["placebo"]["bytes"] != real["bytes"]:
        raise InvalidRun(f"case {record.get('id')!r}: placebo universe exhausted before matching budget")
    if record.get("hits") != real["hits"] or record.get("first_hit_rank") != real["first_hit_rank"]:
        raise InvalidRun(f"case {record.get('id')!r}: real hits disagree with control arm")
