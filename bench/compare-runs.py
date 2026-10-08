#!/usr/bin/env python3
"""Paired baseline-vs-candidate comparison for codesage-bench-runner results.

Input: two result files written by `codesage-bench-runner --results-json`. The
current envelope is `{"meta": {...}, "complete": bool, "records": [...]}`;
a bare JSON list of records (the pre-envelope shape) is also accepted. Each
record is `{id, query, expected_files, hits, first_hit_rank, source, repo}`
plus `error: "timeout" | "rc=N" | "error"` when that case's search did not
run cleanly. Several files per arm may be given and are concatenated. Cases
are paired by `id`, so both arms must come from the same corpus, the same
`--split` / `--salt`, the same `--limit`, and the same project HEAD;
`meta.corpus`, `meta.corpus_sha256`, `meta.split`, `meta.salt`, `meta.limit`,
and `meta.head` are compared and a mismatch is refused unless
`--allow-mismatch`. A file whose run did not finish (`complete: false`) is
refused unless `--allow-partial`.

Controlled input requires retrieval-controls-v2 evidence. Ranking/model
environment, private-runtime policy, and exact runner/helper SHA-256 identity
must agree; mixed instruments within an arm also require --allow-mismatch.
Each paired case retains its originating file's provenance. Compatible
multi-project inputs may use different file orders and partitions.
Required evidence is shape-validated before scoring; null, empty, or malformed
digests, fingerprints, manifests, and file pins cannot be overridden. No split
uses null split/salt, no reranker uses an empty artifact list, and an empty
environment records no overrides. Mixed-setting exceptions retain exact
settings and every affected case/input origin on both arms.
Semantic fingerprints require the supported v4 schema, consistent model/device
identity, and valid pooling, pipeline, sequence, normalization, chunk, and ORT
settings. Dynamic ORT requires a runtime byte pin; static ORT carries its build
identity. Every controlled record must declare its text query.
The comparator does not implicitly upgrade earlier control protocols.

Search failures (records carrying `error`, or `meta.search_failures > 0`) are
scored as misses by the runner, so a flaky arm manufactures lift for the other
one. Failed ids are counted per arm and listed under the Arms table. The
comparison is refused unless `--allow-failures` (compare anyway, failures
stay in) or `--exclude-failed` (drop every id that failed in EITHER arm from
BOTH arms before pairing, keeping the comparison symmetric). The two flags
combine. Exclusion only clears failures it can attribute to records: if
`meta.search_failures` exceeds the records carrying `error` in that arm, the
remainder still refuses without `--allow-failures`.

Per arm: miss rate, recall@5, recall@10, MRR, median first-hit rank (hits only,
upper median, as the runner prints it). Recall@k is |top-k ∩ expected| /
|expected| per case, identical to the runner's scorecard, so it is 0/1 for
single-target cases and fractional otherwise.

Paired deltas (candidate - baseline) per case for recall@10 and MRR, with a
seeded bootstrap 95% interval over clusters chosen by `--cluster-key`:

  case           every case is its own cluster: a plain paired bootstrap.
                 DEFAULT. Honest for the corpora this repo generates today,
                 which carry a single `source` value (or none) per file.
  source-prefix  the `source` text before the first `:` (e.g. `cochange`,
                 `known-item`, `git`). Only meaningful when cases inside one
                 source are correlated (drawn from the same commit, the same
                 symbol family) and the corpus has several sources.
  repo           the record's `repo` field; for concatenated multi-repo runs.

Clusters are resampled with replacement `--bootstrap` times under `--seed`.
The per-draw statistic is the pooled estimator (sum of the drawn clusters'
deltas over the number of drawn cases), and the printed point estimate is the
same statistic on the un-resampled data, which equals the plain case mean.
Under `source-prefix` / `repo` with unequal cluster sizes the interval is
therefore for the pooled estimator resampled by cluster, not for the mean of
per-cluster means. The 2.5th/97.5th nearest-rank percentiles are reported.
Fewer than 5 clusters makes the interval a resample of a handful of means, so
the comparison is refused (exit 2) instead of printing a verdict; pick a finer
`--cluster-key`, or add cases when already on `case`.

Verdict (always printed once the inputs qualify; enforced as the exit code
only with `--gate`):

  ACCEPT iff  mean recall@10 delta >= +0.02
          and bootstrap 2.5th percentile of recall@10 delta > 0
          and mean MRR delta >= -0.005
          and miss-rate delta <= +0.005
          and no cluster with n >= 5 has mean recall@10 delta < -0.02
  else REJECT, naming every failing clause.

Clusters with fewer than 5 cases are listed but cannot veto; under
`--cluster-key case` the clause is vacuous and is reported as such.

Exit codes: 0 on ACCEPT (or on any verdict without `--gate`), 1 on REJECT with
`--gate`, 2 when the comparison is refused: paired n < `--min-n`, fewer than 5
clusters, duplicate or missing ids, unreadable input, recorded search failures
without `--allow-failures`, a partial run without
`--allow-partial`, a provenance mismatch without `--allow-mismatch`, or a
usage error.

Held-out protocol: `split_of(case_id, salt)` assigns each case to `train` or
`heldout` from the first byte of sha256(salt + "\\0" + case_id). Tune on
`train` as often as you like, run `heldout` once per salt, and record the salt
with the result; a heldout number quoted without its salt is not reproducible.
`--split-report <corpus.yaml> --salt <str>` prints the per-split counts.
The runner implements the same function behind `--split` / `--salt`.

Usage:
  compare-runs.py --baseline a.json [a2.json ...] --candidate b.json [b2.json ...]
      [--cluster-key case|source-prefix|repo] [--min-n 30] [--bootstrap 10000]
      [--seed 0] [--gate] [--allow-partial] [--allow-mismatch]
      [--allow-failures] [--exclude-failed]
  compare-runs.py --split-report corpus.yaml --salt STR

`--bootstrap` must be >= 200 (an interval from fewer draws is noise); below
1000 a warning is printed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
import re
import sys
from pathlib import Path
from typing import NoReturn

sys.path.append(str(Path(__file__).resolve().parent))
from _retrieval_controls import (
    CONTROL_PROTOCOL,
    QUERY_RUNTIME,
    InvalidRun,
    digest_json,
    validate_record,
)

RECALL_K = (5, 10)
GATE_K = 10

MIN_R10_DELTA = 0.02
MIN_MRR_DELTA = -0.005
MAX_MISS_DELTA = 0.005
MIN_CLUSTER_R10_DELTA = -0.02
MIN_CLUSTERS_FOR_INTERVAL = 5
MIN_CLUSTER_SIZE_FOR_VETO = 5
PROVENANCE_KEYS = ("corpus", "corpus_sha256", "split", "salt", "limit", "head",
                   "model", "reranker", "device", "control_protocol", "control_seed",
                   "runner_sha256", "controls_sha256")
CONTROL_PROVENANCE_KEYS = (
    "source_sha256", "eligible_files_sha256", "chunks_sha256", "semantic_files_sha256",
    "semantic_fingerprint", "artifact_digest", "embedding_artifacts", "reranker_artifacts",
    "config_sha256", "rg_binary", "environment", "query_runtime",
)
CONTROL_DIGEST_KEYS = (
    "source_sha256", "eligible_files_sha256", "chunks_sha256", "semantic_files_sha256",
    "artifact_digest", "config_sha256",
)
REQUIRED_CONTEXT_KEYS = (*CONTROL_PROVENANCE_KEYS, "binary", "source_manifest", "eligible_files",
                         "head", "model", "reranker", "device")
MIN_BOOTSTRAP = 200
WARN_BOOTSTRAP = 1000
DISPLAY_META_KEYS = (
    "corpus", "corpus_sha256", "split", "salt", "limit", "head", "model", "reranker",
    "codesage", "build_target", "features", "device", "run_at", "search_failures",
)


class Refused(Exception):
    """The comparison cannot be made honestly; carries the reason."""


class MultiValue(list):
    """The distinct values one meta key took across the files of one arm.

    A private type produced only by `load_records`, so a genuinely list-valued
    meta key (which stays a plain `list`) can never be mistaken for an
    accumulation. Never equal to a plain list, even with the same items.
    """

    def __eq__(self, other):
        return isinstance(other, MultiValue) and list.__eq__(self, other)

    def __ne__(self, other):
        return not self.__eq__(other)

    __hash__ = None

    def __repr__(self):
        return "multi" + list.__repr__(self)


def parse_failure_count(value, where: str) -> int:
    if value is None:
        return 0
    bad = Refused(f"{where}: meta.search_failures is not a nonnegative integer ({value!r})")
    if isinstance(value, bool):
        raise bad
    try:
        count = int(value)
    except (TypeError, ValueError, OverflowError):
        raise bad
    if count < 0 or (isinstance(value, float) and value != count):
        raise bad
    return count


def split_of(case_id: str, salt: str) -> str:
    digest = hashlib.sha256((salt + "\0" + case_id).encode("utf-8")).digest()
    return "train" if digest[0] < 128 else "heldout"


def split_report(corpus_path: Path, salt: str) -> list[str]:
    try:
        import yaml
    except ImportError:
        raise Refused("pyyaml required for --split-report: pip install pyyaml")
    try:
        corpus = yaml.safe_load(corpus_path.read_text())
    except OSError as e:
        raise Refused(f"cannot read {corpus_path}: {e}")
    except yaml.YAMLError as e:
        raise Refused(f"{corpus_path}: malformed YAML: {e}")
    if not isinstance(corpus, dict) or not isinstance(corpus.get("cases"), list):
        raise Refused(f"{corpus_path}: expected a mapping with a `cases` list")
    ids: list[str] = []
    for i, case in enumerate(corpus["cases"]):
        if not isinstance(case, dict) or "id" not in case:
            raise Refused(f"{corpus_path}: case #{i} has no `id`")
        ids.append(str(case["id"]))
    counts = {"train": 0, "heldout": 0}
    for cid in ids:
        counts[split_of(cid, salt)] += 1
    total = len(ids)
    lines = [f"# Split report: {corpus_path.name} (salt={salt!r})", ""]
    for name in ("train", "heldout"):
        share = counts[name] / total if total else 0.0
        lines.append(f"- {name}: {counts[name]} of {total} ({share:.1%})")
    return lines


def valid_digest(value) -> bool:
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def valid_integer(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def valid_file_pin(value) -> bool:
    return (isinstance(value, dict) and isinstance(value.get("path"), str)
            and "\0" not in value["path"] and Path(value["path"]).is_absolute()
            and bool(Path(value["path"]).name) and ".." not in Path(value["path"]).parts
            and valid_digest(value.get("sha256")) and valid_integer(value.get("size"))
            and value["size"] > 0)


def valid_source_path(value) -> bool:
    return (isinstance(value, str) and bool(value) and "\0" not in value
            and not Path(value).is_absolute() and bool(Path(value).name)
            and not ({"..", "."} & set(Path(value).parts)))


def normalized_device(value) -> str | None:
    if not isinstance(value, str):
        return None
    device = value.strip().lower()
    return "cuda" if device in ("gpu", "cuda") else device if device in ("cpu", "coreml") else None


def valid_unsigned_text(value: str, *, bits: int = 64, positive: bool = True) -> bool:
    return (re.fullmatch(r"0|[1-9][0-9]{0,19}", value) is not None
            and (0 if not positive else 1) <= int(value) < 2 ** bits)


def validate_semantic_fingerprint(provenance: dict, model: str, device: str, where: str) -> dict:
    def invalid(field) -> NoReturn:
        raise Refused(f"{where}: invalid controlled-run provenance.semantic_fingerprint {field}")

    fingerprint = provenance["semantic_fingerprint"]
    if (not isinstance(fingerprint, str) or "\0" in fingerprint
            or re.fullmatch(r"v4;[^\r\n]+", fingerprint) is None):
        invalid("(unsupported or malformed version)")
    parts = fingerprint.split(";")[1:]
    if any("=" not in part or not all(part.split("=", 1)) for part in parts):
        invalid("fields")
    fields = dict(part.split("=", 1) for part in parts)
    required = {"model", "artifacts", "dim", "pooling", "device", "ort", "pipeline",
                "maxseq", "norm", "chunker", "chunk"}
    if len(fields) != len(parts) or set(fields) != required:
        invalid("fields (requires the complete v4 schema)")
    if fields["model"] != model or fields["artifacts"] != provenance["artifact_digest"]:
        invalid("model/artifacts (disagrees with metadata or pins)")
    if fields["device"] != normalized_device(device):
        invalid("device (disagrees with configured provider)")
    if fields["pooling"] not in ("mean", "cls") or fields["norm"] not in ("l2", "none"):
        invalid("pooling/norm")
    for name in ("dim", "maxseq", "pipeline", "chunker"):
        if not valid_unsigned_text(fields[name], bits=32 if name in ("pipeline", "chunker") else 64):
            invalid(name)
    chunk = fields["chunk"].split("/")
    if len(chunk) != 3 or any(not valid_unsigned_text(value, positive=i == 0) for i, value in enumerate(chunk)):
        invalid("chunk")
    runtime = re.fullmatch(r"api1\.(0|[1-9][0-9]{0,9})/(dylib|static:[0-9a-f]{16})", fields["ort"])
    if runtime is None or not valid_unsigned_text(runtime[1], bits=32, positive=False):
        invalid("ort")
    dynamic = runtime[2] == "dylib"
    if dynamic and fields["device"] == "coreml":
        invalid("ort/device (CoreML requires the statically linked Apple runtime)")
    labels = [pin["label"] for pin in provenance["embedding_artifacts"]]
    expected = ["tokenizer", "onnx"]
    if "onnx_data" in labels:
        expected.append("onnx_data")
    if dynamic:
        expected.append("ort_runtime")
    if labels != expected:
        invalid("ort/artifact labels (requires the producer's runtime mode and artifact order)")
    return fields


def validate_controlled_record(record: dict, fingerprint: dict | None = None) -> None:
    if not isinstance(record.get("query"), str):
        raise InvalidRun(f"case {record.get('id')!r}: missing or non-text controlled query")
    validate_record(record)
    if fingerprint is None:
        return
    stderr = record["arms"]["codesage"]["runtime"]["stderr"]
    if "CODESAGE_ALLOW_CPU_FALLBACK" in stderr:
        raise InvalidRun(f"case {record['id']!r}: degraded CPU fallback runtime")
    for line in stderr.splitlines():
        for marker, names in (("loading embedding model", ("model",)),
                              ("embedding model loaded", ("dim", "pooling", "execution_provider"))):
            if marker not in line:
                continue
            for name in names:
                value = re.search(rf'\b{name}=(?:"([^"\n]*)"|([^\s]+))', line)
                if value is None:
                    continue
                observed = value[1] if value[1] is not None else value[2]
                field = "device" if name == "execution_provider" else name
                observed = observed.lower() if name == "pooling" else observed
                if observed != fingerprint[field]:
                    raise InvalidRun(f"case {record['id']!r}: runtime {name}={observed!r} "
                                     f"disagrees with semantic fingerprint {field}={fingerprint[field]!r}")


def validate_controlled_metadata(meta: dict, where: str) -> dict:
    def invalid(field):
        raise Refused(f"{where}: invalid controlled-run {field}")

    if meta.get("control_protocol") != CONTROL_PROTOCOL:
        raise Refused(f"{where}: unknown control protocol")
    missing = [key for key in PROVENANCE_KEYS if key not in meta]
    if missing:
        raise Refused(f"{where}: missing controlled-run metadata: {', '.join(missing)}")
    for key in ("corpus_sha256", "runner_sha256", "controls_sha256"):
        if not valid_digest(meta[key]):
            invalid(key)
    for key in ("corpus", "model", "reranker"):
        if (not isinstance(meta[key], str) or not meta[key].strip()
                or (key != "corpus" and meta[key].lower() in ("unknown", "null")) or "\0" in meta[key]):
            invalid(key)
    if meta["model"] == "none":
        invalid("model")
    if (not isinstance(meta["head"], str) or (meta["head"] != "not-a-git-repo"
            and re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", meta["head"]) is None)):
        invalid("head")
    if normalized_device(meta["device"]) is None:
        invalid("device")
    if not valid_integer(meta["limit"]) or meta["limit"] < 1:
        invalid("limit")
    if not valid_integer(meta["control_seed"]):
        invalid("control_seed")
    if (meta["split"] not in (None, "train", "heldout")
            or (meta["split"] is None and meta["salt"] is not None)
            or (meta["split"] is not None and (not isinstance(meta["salt"], str)
                or re.fullmatch(r"[A-Za-z0-9_.-]+", meta["salt"]) is None))):
        invalid("split/salt")
    provenance = meta.get("provenance")
    if not isinstance(provenance, dict):
        raise Refused(f"{where}: missing controlled-run provenance")
    missing = [key for key in REQUIRED_CONTEXT_KEYS if key not in provenance]
    if missing:
        raise Refused(f"{where}: missing controlled-run provenance: {', '.join(missing)}")
    for key in CONTROL_DIGEST_KEYS:
        if not valid_digest(provenance[key]):
            invalid(f"provenance.{key}")
    for key in ("head", "model", "reranker"):
        if provenance[key] != meta[key]:
            invalid(f"provenance.{key} (disagrees with metadata)")
    if normalized_device(provenance["device"]) != normalized_device(meta["device"]):
        invalid("provenance.device (disagrees with metadata)")
    for key in ("binary", "rg_binary"):
        if not valid_file_pin(provenance[key]):
            invalid(f"provenance.{key}")
    embedding = provenance["embedding_artifacts"]
    if (not isinstance(embedding, list) or not embedding
            or any(not valid_file_pin(pin) or not isinstance(pin.get("label"), str)
                   or re.fullmatch(r"[a-z][a-z0-9_]*", pin["label"]) is None for pin in embedding)):
        invalid("provenance.embedding_artifacts")
    labels = [pin["label"] for pin in embedding]
    if len(labels) != len(set(labels)) or not {"tokenizer", "onnx"}.issubset(labels):
        invalid("provenance.embedding_artifacts labels")
    digest = hashlib.sha256("".join(f"{pin['label']}={pin['sha256']}\n" for pin in embedding).encode()).hexdigest()
    if provenance["artifact_digest"] != digest:
        invalid("provenance.artifact_digest (disagrees with artifact pins)")
    reranker = provenance["reranker_artifacts"]
    if (not isinstance(reranker, list) or any(not valid_file_pin(pin) for pin in reranker)
            or (meta["reranker"] != "none" and len({pin["path"] for pin in reranker}) < 2)):
        invalid("provenance.reranker_artifacts")
    fingerprint = validate_semantic_fingerprint(provenance, meta["model"], meta["device"], where)
    eligible = provenance["eligible_files"]
    if (not isinstance(eligible, list) or not eligible or any(not valid_source_path(p) for p in eligible)
            or len(eligible) != len(set(eligible))):
        invalid("provenance.eligible_files")
    manifest = provenance["source_manifest"]
    if (not isinstance(manifest, list) or not manifest
            or any(not isinstance(pin, dict) or not valid_source_path(pin.get("path"))
                   or not valid_digest(pin.get("sha256")) for pin in manifest)
            or len({pin["path"] for pin in manifest}) != len(manifest)
            or {pin["path"] for pin in manifest} != set(eligible)):
        invalid("provenance.source_manifest")
    for key, value in (("source_sha256", manifest), ("eligible_files_sha256", eligible)):
        if provenance[key] != digest_json(value):
            invalid(f"provenance.{key} (disagrees with retained manifest)")
    environment = provenance["environment"]
    if (not isinstance(environment, dict) or any(
            not isinstance(k, str) or not k or not isinstance(v, str) for k, v in environment.items())):
        invalid("provenance.environment")
    if provenance["query_runtime"] != QUERY_RUNTIME:
        invalid("provenance.query_runtime")
    return fingerprint


def load_records(paths: list[Path], *, allow_partial: bool) -> tuple[dict, dict[str, dict]]:
    """Concatenate result files into (meta, id-keyed records).

    Accepts the `{"meta", "complete", "records"}` envelope or a bare list.
    Meta values from several files are merged; a key that differs across the
    files of one arm is recorded as a `MultiValue` (sorted by canonical JSON,
    originals kept) for display. Controlled records retain the originating
    metadata independently of the merged display metadata.
    """
    out: dict[str, dict] = {}
    meta: dict = {}
    input_controlled: bool | None = None
    for path in paths:
        try:
            data = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as e:
            raise Refused(f"cannot read {path}: {e}")
        file_meta: dict = {}
        completion = None
        if isinstance(data, dict):
            if "records" in data:
                completion = data.get("complete")
                if data.get("complete") is False and not allow_partial:
                    raise Refused(
                        f"{path}: run did not finish (complete: false); "
                        "pass --allow-partial to compare anyway"
                    )
                file_meta = data.get("meta") or {}
                if not isinstance(file_meta, dict):
                    raise Refused(f"{path}: meta must be a mapping")
                data = data["records"]
            else:
                data = data.get("cases", data.get("results"))
        if not isinstance(data, list):
            raise Refused(f"{path}: expected a JSON list of case records or a results envelope")
        controlled = (any(key in file_meta for key in ("control_protocol", "control_seed", "runner_sha256", "controls_sha256"))
                      or any(isinstance(record, dict) and "arms" in record for record in data))
        if input_controlled is not None and controlled != input_controlled:
            raise Refused(f"{path}: cannot concatenate controlled and legacy inputs")
        input_controlled = controlled
        if controlled:
            if not isinstance(completion, bool):
                raise Refused(f"{path}: invalid controlled-run completion flag")
            fingerprint = validate_controlled_metadata(file_meta, str(path))
        for i, rec in enumerate(data):
            if not isinstance(rec, dict) or rec.get("id") is None:
                raise Refused(f"{path}: record #{i} has no `id`")
            cid = str(rec["id"])
            if cid in out:
                raise Refused(f"{path}: duplicate case id {cid!r} within one arm")
            if file_meta.get("control_protocol"):
                try:
                    validate_controlled_record(rec, fingerprint)
                    if rec["arms"]["codesage"]["runtime"]["reranker"] != file_meta["reranker"]:
                        raise InvalidRun(f"case {cid!r}: runtime reranker disagrees with metadata")
                    eligible = set(file_meta["provenance"]["eligible_files"])
                    if (not set(rec["expected_files"]).issubset(eligible)
                            or any(not set(arm["hits"]).issubset(eligible) for arm in rec["arms"].values())):
                        raise InvalidRun(f"case {cid!r}: files outside pinned eligible universe")
                except InvalidRun as exc:
                    raise Refused(f"{path}: {exc}") from exc
            out[cid] = {**rec, "_input_provenance": {
                **{key: file_meta[key] for key in PROVENANCE_KEYS},
                "provenance": dict(file_meta["provenance"]),
            }, "_input_path": str(path)} if controlled else rec
        for k, v in file_meta.items():
            if k == "search_failures":
                # Validate each input before aggregation, including the first,
                # so invalid counts cannot cancel another file's failures.
                meta[k] = meta.get(k, 0) + parse_failure_count(v, str(path))
            elif k not in meta:
                meta[k] = v
            elif meta[k] != v:
                prior = list(meta[k]) if isinstance(meta[k], MultiValue) else [meta[k]]
                seen = {json.dumps(x, sort_keys=True): x for x in [*prior, v]}
                meta[k] = MultiValue(seen[s] for s in sorted(seen))
    return meta, out


def failed_ids(records: dict[str, dict]) -> list[str]:
    return sorted(cid for cid, rec in records.items() if rec.get("error") is not None)


def search_failure_counts(
    base_meta: dict, cand_meta: dict, baseline: dict[str, dict], candidate: dict[str, dict],
    *, attributed: dict[str, int] | None = None,
) -> dict[str, int]:
    """Per-arm failure count: the larger of meta.search_failures (minus the
    failures already attributed to dropped records, floor 0) and the records
    still carrying `error` (a legacy list has no meta)."""
    attributed = attributed or {}
    counts = {}
    for arm, meta, recs in (("baseline", base_meta, baseline), ("candidate", cand_meta, candidate)):
        from_meta = parse_failure_count(meta.get("search_failures"), f"{arm} meta")
        remaining = max(0, from_meta - attributed.get(arm, 0))
        counts[arm] = max(remaining, len(failed_ids(recs)))
    return counts


def exclude_failed(
    baseline: dict[str, dict], candidate: dict[str, dict]
) -> tuple[dict[str, dict], dict[str, dict], list[str]]:
    """Drop every id that failed in either arm from both arms."""
    dropped = sorted(set(failed_ids(baseline)) | set(failed_ids(candidate)))
    drop = set(dropped)
    return (
        {k: v for k, v in baseline.items() if k not in drop},
        {k: v for k, v in candidate.items() if k not in drop},
        dropped,
    )


def provenance_mismatch(base_meta: dict, cand_meta: dict) -> list[str]:
    diffs = mixed_instrument_mismatch(base_meta, cand_meta)
    for key in PROVENANCE_KEYS:
        b, c = base_meta.get(key), cand_meta.get(key)
        if b != c:
            diffs.append(f"{key}: baseline {b!r} vs candidate {c!r}")
    if base_meta.get("control_protocol") or cand_meta.get("control_protocol"):
        for key in CONTROL_PROVENANCE_KEYS:
            values = []
            for meta in (base_meta, cand_meta):
                provenance = meta.get("provenance") or {}
                if isinstance(provenance, MultiValue):
                    values.append(sorted((p.get(key) for p in provenance), key=lambda v: json.dumps(v, sort_keys=True)))
                else:
                    values.append(provenance.get(key))
            b, c = values
            if b != c:
                diffs.append(f"provenance.{key}: baseline {b!r} vs candidate {c!r}")
    return diffs


def mixed_instrument_mismatch(base_meta: dict, cand_meta: dict) -> list[str]:
    diffs = []
    for label, meta in (("baseline", base_meta), ("candidate", cand_meta)):
        for key in ("runner_sha256", "controls_sha256"):
            value = meta.get(key)
            if isinstance(value, MultiValue):
                diffs.append(f"{key}: {label} mixes instrument identities {value!r}")
        provenance = meta.get("provenance")
        if isinstance(provenance, MultiValue):
            for key in ("environment", "query_runtime"):
                if len({json.dumps(p.get(key), sort_keys=True) for p in provenance}) > 1:
                    diffs.append(f"provenance.{key}: {label} mixes instrument settings")
    return diffs


def mixed_origin_mismatch(records: dict[str, dict], label: str) -> list[str]:
    origins = {}
    for cid, record in records.items():
        origin = record.get("_input_provenance")
        if not isinstance(origin, dict):
            raise Refused(f"{label} case {cid!r}: missing originating controlled-run provenance")
        origins[cid] = origin
    fields = {key: {cid: origin[key] for cid, origin in origins.items()}
              for key in ("runner_sha256", "controls_sha256")}
    for group in ("environment", "query_runtime"):
        names = set().union(*(origin["provenance"][group] for origin in origins.values()))
        for name in sorted(names):
            fields[f"provenance.{group}.{name}"] = {
                cid: {"present": name in origin["provenance"][group],
                      "value": origin["provenance"][group].get(name)} for cid, origin in origins.items()}
    differences = []
    for field, values in fields.items():
        if len({json.dumps(value, sort_keys=True) for value in values.values()}) < 2:
            continue
        kind = "identities" if field.endswith("sha256") else "settings"
        for cid, value in sorted(values.items()):
            if isinstance(value, dict):
                rendered = repr(value["value"]) if value["present"] else "<unset>"
            else:
                rendered = repr(value)
            differences.append(f"{label} mixes instrument {kind}: case {cid!r}, "
                               f"input {records[cid]['_input_path']!r}, {field}={rendered}")
    return differences


def paired_provenance_mismatch(
    base_meta: dict, cand_meta: dict, baseline: dict[str, dict], candidate: dict[str, dict],
) -> list[str]:
    if not (base_meta.get("control_protocol") and cand_meta.get("control_protocol")):
        return provenance_mismatch(base_meta, cand_meta)
    diffs = [*mixed_origin_mismatch(baseline, "baseline"), *mixed_origin_mismatch(candidate, "candidate")]
    for cid in sorted(baseline.keys() & candidate.keys()):
        base_origin = baseline[cid].get("_input_provenance")
        cand_origin = candidate[cid].get("_input_provenance")
        if not isinstance(base_origin, dict) or not isinstance(cand_origin, dict):
            raise Refused(f"case {cid!r}: missing originating controlled-run provenance")
        for difference in provenance_mismatch(base_origin, cand_origin):
            diffs.append(f"case {cid!r}: {difference} "
                         f"(baseline input {baseline[cid]['_input_path']!r}; "
                         f"candidate input {candidate[cid]['_input_path']!r})")
    return diffs


def first_hit_rank(rec: dict) -> int | None:
    rank = rec.get("first_hit_rank")
    if rank is not None:
        bad = Refused(
            f"record {rec.get('id')!r}: first_hit_rank must be a positive integer ({rank!r})"
        )
        if isinstance(rank, bool):
            raise bad
        try:
            value = int(rank)
        except (TypeError, ValueError):
            raise bad
        if isinstance(rank, float) and rank != value:
            raise bad
        if value < 1:
            raise bad
        return value
    expected = set(rec.get("expected_files", []))
    for i, path in enumerate(rec.get("hits", []), start=1):
        if path in expected:
            return i
    return None


def recall_at(rec: dict, k: int) -> float:
    expected = set(rec.get("expected_files", []))
    if not expected:
        return 0.0
    return len(set(rec.get("hits", [])[:k]) & expected) / len(expected)


def mrr_of(rec: dict) -> float:
    rank = first_hit_rank(rec)
    return 1.0 / rank if rank else 0.0


def score(rec: dict) -> dict:
    rank = first_hit_rank(rec)
    return {
        "first_hit_rank": rank,
        "miss": 0.0 if rank else 1.0,
        "mrr": mrr_of(rec),
        **{f"recall@{k}": recall_at(rec, k) for k in RECALL_K},
    }


def cluster_of(rec: dict, key: str) -> str:
    if key == "case":
        return str(rec["id"])
    if key == "repo":
        return str(rec.get("repo") or "<no-repo>")
    source = str(rec.get("source") or "?")
    return source.split(":", 1)[0]


def mean(xs: list[float]) -> float:
    return sum(xs) / len(xs) if xs else 0.0


def median_first_hit(scores: list[dict]) -> int | None:
    ranks = sorted(s["first_hit_rank"] for s in scores if s["first_hit_rank"])
    return ranks[len(ranks) // 2] if ranks else None


def arm_summary(scores: list[dict]) -> dict:
    return {
        "miss_rate": mean([s["miss"] for s in scores]),
        **{f"recall@{k}": mean([s[f"recall@{k}"] for s in scores]) for k in RECALL_K},
        "mrr": mean([s["mrr"] for s in scores]),
        "median_first_hit": median_first_hit(scores),
    }


def percentile(sorted_xs: list[float], q: float) -> float:
    """Nearest-rank percentile: the ceil(q*n)-th smallest value (1-based)."""
    n = len(sorted_xs)
    if n == 0:
        return 0.0
    idx = max(0, math.ceil(q * n) - 1)
    return sorted_xs[min(idx, n - 1)]


def pooled(clusters: dict[str, list[float]]) -> float:
    """Ratio of sums over all clusters: the statistic the bootstrap resamples."""
    count = sum(len(v) for v in clusters.values())
    return sum(sum(v) for v in clusters.values()) / count if count else 0.0


def clustered_bootstrap(
    clusters: dict[str, list[float]], draws: int, seed: int
) -> dict:
    """Resample clusters with replacement; return mean and 2.5/97.5 percentiles.

    Each draw's statistic is `pooled()` of the drawn clusters (ratio of sums),
    so the point estimate reported alongside must be `pooled(clusters)`.
    """
    names = sorted(clusters)
    sums = [sum(clusters[n]) for n in names]
    sizes = [len(clusters[n]) for n in names]
    rng = random.Random(seed)
    k = len(names)
    means: list[float] = []
    for _ in range(draws):
        total = 0.0
        count = 0
        for _ in range(k):
            j = rng.randrange(k)
            total += sums[j]
            count += sizes[j]
        means.append(total / count if count else 0.0)
    means.sort()
    return {
        "mean": mean(means),
        "lb": percentile(means, 0.025),
        "ub": percentile(means, 0.975),
        "clusters": k,
        "draws": draws,
    }


def verdict(
    r10_delta: float,
    r10_lb: float,
    mrr_delta: float,
    miss_delta: float,
    cluster_r10: dict[str, float],
    cluster_sizes: dict[str, int],
) -> tuple[bool, list[str]]:
    failing: list[str] = []
    if not r10_delta >= MIN_R10_DELTA:
        failing.append(
            f"mean recall@{GATE_K} delta {r10_delta:+.4f} < {MIN_R10_DELTA:+.4f}"
        )
    if not r10_lb > 0:
        failing.append(
            f"bootstrap lower bound of recall@{GATE_K} delta {r10_lb:+.4f} <= 0"
        )
    if not mrr_delta >= MIN_MRR_DELTA:
        failing.append(f"mean MRR delta {mrr_delta:+.4f} < {MIN_MRR_DELTA:+.4f}")
    if not miss_delta <= MAX_MISS_DELTA:
        failing.append(f"miss-rate delta {miss_delta:+.4f} > {MAX_MISS_DELTA:+.4f}")
    regressed = sorted(
        (name, d)
        for name, d in cluster_r10.items()
        if cluster_sizes[name] >= MIN_CLUSTER_SIZE_FOR_VETO and d < MIN_CLUSTER_R10_DELTA
    )
    if regressed:
        detail = ", ".join(f"{name} {d:+.4f}" for name, d in regressed)
        failing.append(
            f"per-cluster recall@{GATE_K} delta < {MIN_CLUSTER_R10_DELTA:+.4f} "
            f"(clusters with n >= {MIN_CLUSTER_SIZE_FOR_VETO}): {detail}"
        )
    return (not failing), failing


def fmt_median(v: int | None) -> str:
    return str(v) if v is not None else "MISS"


def fmt_meta(meta: dict) -> str:
    if not meta:
        return "(no meta: legacy list input)"
    parts = [f"{k}={meta[k]!r}" for k in DISPLAY_META_KEYS if k in meta]
    return ", ".join(parts) if parts else "(meta present, no provenance keys)"


def compare(
    baseline: dict[str, dict],
    candidate: dict[str, dict],
    *,
    cluster_key: str,
    min_n: int,
    draws: int,
    seed: int,
    base_meta: dict | None = None,
    cand_meta: dict | None = None,
    excluded: list[str] | None = None,
    mismatch_overrides: list[str] | None = None,
) -> tuple[list[str], bool]:
    """Return (report lines, accept); raise Refused when no honest verdict exists."""
    ids = sorted(set(baseline) & set(candidate))
    controlled = (base_meta or {}).get("control_protocol") or (cand_meta or {}).get("control_protocol")
    if controlled:
        if not all((meta or {}).get("control_protocol") for meta in (base_meta, cand_meta)):
            raise Refused("controlled comparison requires controlled inputs for both arms")
        if set(baseline) != set(candidate):
            raise Refused("controlled arms must contain identical case ids")
        for cid in ids:
            try:
                validate_controlled_record(baseline[cid])
                validate_controlled_record(candidate[cid])
            except InvalidRun as exc:
                raise Refused(str(exc)) from exc
            if any(baseline[cid].get(k) != candidate[cid].get(k) for k in ("query", "expected_files")):
                raise Refused(f"case {cid!r}: query or gold differs between controlled arms")
    n = len(ids)
    lines: list[str] = ["# CodeSage paired comparison", ""]
    lines.append(f"- Baseline: {fmt_meta(base_meta or {})}")
    lines.append(f"- Candidate: {fmt_meta(cand_meta or {})}")
    if mismatch_overrides:
        lines.append("- Provenance gate overridden with `--allow-mismatch`; these inputs have differing settings or instrument identities.")
        lines.extend(f"- Allowed mismatch: {difference}" for difference in mismatch_overrides)
    if not controlled:
        lines.append("- Legacy uncontrolled inputs: this verdict compares retained ranks; "
                     "placebo advantage and source/index/model provenance are unverified.")
    base_failed = failed_ids(baseline)
    cand_failed = failed_ids(candidate)
    if excluded:
        lines.append(f"- Excluded failed ids (both arms): {len(excluded)} ({', '.join(excluded)})")
    else:
        lines.append("- Excluded failed ids (both arms): none")
    lines.append(
        f"- Paired cases: {n} (baseline {len(baseline)}, candidate {len(candidate)}, "
        f"baseline-only {len(set(baseline) - set(candidate))}, "
        f"candidate-only {len(set(candidate) - set(baseline))})"
    )
    if n < min_n:
        raise Refused(f"paired n={n} is below --min-n {min_n}")

    base_scores = [score(baseline[i]) for i in ids]
    cand_scores = [score(candidate[i]) for i in ids]
    base = arm_summary(base_scores)
    cand = arm_summary(cand_scores)

    r10_key = f"recall@{GATE_K}"
    r10_deltas = [c[r10_key] - b[r10_key] for b, c in zip(base_scores, cand_scores)]
    mrr_deltas = [c["mrr"] - b["mrr"] for b, c in zip(base_scores, cand_scores)]
    miss_delta = cand["miss_rate"] - base["miss_rate"]

    r10_clusters: dict[str, list[float]] = {}
    mrr_clusters: dict[str, list[float]] = {}
    for cid, d10, dmrr in zip(ids, r10_deltas, mrr_deltas):
        name = cluster_of(baseline[cid], cluster_key)
        r10_clusters.setdefault(name, []).append(d10)
        mrr_clusters.setdefault(name, []).append(dmrr)
    cluster_r10 = {name: mean(v) for name, v in r10_clusters.items()}
    cluster_sizes = {name: len(v) for name, v in r10_clusters.items()}
    # Same statistic the bootstrap resamples, evaluated on the original sample.
    r10_delta = pooled(r10_clusters)
    mrr_delta = pooled(mrr_clusters)

    if len(r10_clusters) < MIN_CLUSTERS_FOR_INTERVAL:
        if cluster_key == "case":
            remedy = f"--min-n cannot usefully be below {MIN_CLUSTERS_FOR_INTERVAL}; add cases."
        else:
            remedy = "Use --cluster-key case for a plain paired bootstrap."
        raise Refused(
            f"--cluster-key {cluster_key} yields {len(r10_clusters)} cluster(s) "
            f"({', '.join(sorted(r10_clusters))}); at least {MIN_CLUSTERS_FOR_INTERVAL} "
            f"are needed for a bootstrap interval. {remedy}"
        )

    r10_boot = clustered_bootstrap(r10_clusters, draws, seed)
    mrr_boot = clustered_bootstrap(mrr_clusters, draws, seed)

    lines.append(f"- Cluster key: {cluster_key}; clusters: {len(r10_clusters)}")
    lines.append(f"- Bootstrap: {draws} draws, seed {seed}")
    lines.append("")
    lines.append("## Arms")
    lines.append("")
    lines.append("| metric | baseline | candidate | delta |")
    lines.append("|---|---:|---:|---:|")
    for label, key in (
        ("miss rate", "miss_rate"),
        ("recall@5", "recall@5"),
        ("recall@10", "recall@10"),
        ("MRR", "mrr"),
    ):
        lines.append(
            f"| {label} | {base[key]:.4f} | {cand[key]:.4f} | {cand[key] - base[key]:+.4f} |"
        )
    lines.append(
        f"| median first-hit | {fmt_median(base['median_first_hit'])} "
        f"| {fmt_median(cand['median_first_hit'])} | |"
    )
    lines.append(
        f"| search failures | {len(base_failed)} | {len(cand_failed)} | |"
    )
    lines.append("")
    if base_failed or cand_failed:
        lines.append("Failed searches (scored as misses, still paired):")
        if base_failed:
            lines.append(f"- baseline: {', '.join(base_failed)}")
        if cand_failed:
            lines.append(f"- candidate: {', '.join(cand_failed)}")
        lines.append("")
    lines.append("## Paired deltas (candidate - baseline)")
    lines.append("")
    lines.append("| metric | mean | boot mean | 2.5% | 97.5% |")
    lines.append("|---|---:|---:|---:|---:|")
    lines.append(
        f"| recall@{GATE_K} | {r10_delta:+.4f} | {r10_boot['mean']:+.4f} "
        f"| {r10_boot['lb']:+.4f} | {r10_boot['ub']:+.4f} |"
    )
    lines.append(
        f"| MRR | {mrr_delta:+.4f} | {mrr_boot['mean']:+.4f} "
        f"| {mrr_boot['lb']:+.4f} | {mrr_boot['ub']:+.4f} |"
    )
    lines.append("")
    lines.append(f"## Per-cluster recall@{GATE_K} delta")
    lines.append("")
    if cluster_key == "case":
        lines.append(
            "Every cluster is one case under --cluster-key case; the per-cluster "
            f"veto (n >= {MIN_CLUSTER_SIZE_FOR_VETO}) is vacuous. "
            f"Cases regressing: {sum(1 for d in r10_deltas if d < 0)}, "
            f"improving: {sum(1 for d in r10_deltas if d > 0)}, "
            f"unchanged: {sum(1 for d in r10_deltas if d == 0)}."
        )
    else:
        lines.append("| cluster | n | delta | veto |")
        lines.append("|---|---:|---:|---|")
        for name in sorted(r10_clusters):
            size = cluster_sizes[name]
            veto = "yes" if size >= MIN_CLUSTER_SIZE_FOR_VETO else (
                f"no (n < {MIN_CLUSTER_SIZE_FOR_VETO}, informational)"
            )
            lines.append(f"| {name} | {size} | {cluster_r10[name]:+.4f} | {veto} |")
    lines.append("")

    accept, failing = verdict(
        r10_delta, r10_boot["lb"], mrr_delta, miss_delta, cluster_r10, cluster_sizes
    )
    lines.append("## Verdict")
    lines.append("")
    if cluster_key == "case":
        cluster_clause = " (the per-cluster clause is vacuous under --cluster-key case)"
    else:
        cluster_clause = (
            f" AND no cluster with n >= {MIN_CLUSTER_SIZE_FOR_VETO} has "
            f"recall@{GATE_K} delta < {MIN_CLUSTER_R10_DELTA:+.2f}"
        )
    lines.append(
        f"Predicate: mean recall@{GATE_K} delta >= {MIN_R10_DELTA:+.2f} AND bootstrap "
        f"lower bound > 0 AND mean MRR delta >= {MIN_MRR_DELTA:+.3f} AND miss-rate "
        f"delta <= {MAX_MISS_DELTA:+.3f}{cluster_clause}"
    )
    lines.append("")
    if accept:
        lines.append("ACCEPT")
    else:
        lines.append("REJECT")
        for clause in failing:
            lines.append(f"- {clause}")
    return lines, accept


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    ap.add_argument("--baseline", nargs="+", type=Path, default=None)
    ap.add_argument("--candidate", nargs="+", type=Path, default=None)
    ap.add_argument(
        "--cluster-key", choices=["case", "source-prefix", "repo"], default="case"
    )
    ap.add_argument("--min-n", type=int, default=30)
    ap.add_argument("--bootstrap", type=int, default=10_000)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--gate", action="store_true", help="Exit 1 on REJECT, 0 on ACCEPT.")
    ap.add_argument(
        "--allow-partial", action="store_true",
        help="Compare result files whose run did not finish (complete: false).",
    )
    ap.add_argument(
        "--allow-mismatch", action="store_true",
        help="Compare arms whose meta corpus / corpus_sha256 / split / salt / limit differ.",
    )
    ap.add_argument(
        "--allow-failures", action="store_true",
        help="Compare arms whose run recorded search failures (timeouts, nonzero rc); "
             "the failed cases stay in as misses.",
    )
    ap.add_argument(
        "--exclude-failed", action="store_true",
        help="Drop every id that failed in either arm from both arms before pairing.",
    )
    ap.add_argument("--split-report", type=Path, default=None, metavar="CORPUS_YAML")
    ap.add_argument("--salt", default=None, help="Only with --split-report.")
    args = ap.parse_args(argv)

    if args.split_report is not None:
        if args.salt is None:
            ap.error("--split-report requires --salt")
        if args.baseline or args.candidate:
            ap.error("--split-report cannot be combined with --baseline/--candidate")
        try:
            lines = split_report(args.split_report, args.salt)
        except Refused as e:
            print(f"REFUSED: {e}", file=sys.stderr)
            return 2
        for line in lines:
            print(line)
        return 0

    if args.salt is not None:
        ap.error("--salt is only meaningful with --split-report")
    if not args.baseline or not args.candidate:
        ap.error("--baseline and --candidate are required (or use --split-report)")
    if args.bootstrap < MIN_BOOTSTRAP:
        ap.error(f"--bootstrap must be >= {MIN_BOOTSTRAP} (got {args.bootstrap})")
    if args.bootstrap < WARN_BOOTSTRAP:
        print(f"WARNING: --bootstrap {args.bootstrap} is below {WARN_BOOTSTRAP}; "
              "the interval percentiles are coarse", file=sys.stderr)

    try:
        base_meta, baseline = load_records(args.baseline, allow_partial=args.allow_partial)
        cand_meta, candidate = load_records(args.candidate, allow_partial=args.allow_partial)
        diffs = paired_provenance_mismatch(base_meta, cand_meta, baseline, candidate)
        if diffs and not args.allow_mismatch:
            raise Refused(
                "baseline and candidate provenance differ (" + "; ".join(diffs)
                + "); pass --allow-mismatch to compare anyway"
            )
        excluded: list[str] = []
        attributed: dict[str, int] = {}
        if args.exclude_failed:
            # Only failures attributable to a dropped record are cleared; a
            # meta count above that stays a failure of unknown location.
            attributed = {"baseline": len(failed_ids(baseline)), "candidate": len(failed_ids(candidate))}
            baseline, candidate, excluded = exclude_failed(baseline, candidate)
        failures = search_failure_counts(
            base_meta, cand_meta, baseline, candidate, attributed=attributed
        )
        if any(failures.values()) and not args.allow_failures:
            how = (
                "these failures are recorded in meta but no record carries `error`, "
                "so --exclude-failed cannot locate them. "
                if args.exclude_failed else
                "Rerun, pass --exclude-failed to drop those ids from both arms, or "
            )
            raise Refused(
                f"search failures recorded (baseline {failures['baseline']}, "
                f"candidate {failures['candidate']}); a flaky search arm scores as "
                f"misses and fabricates lift. {how}"
                "--allow-failures compares as is."
            )
        lines, accept = compare(
            baseline,
            candidate,
            cluster_key=args.cluster_key,
            min_n=args.min_n,
            draws=args.bootstrap,
            seed=args.seed,
            base_meta=base_meta,
            cand_meta=cand_meta,
            excluded=excluded,
            mismatch_overrides=diffs if args.allow_mismatch else None,
        )
    except Refused as e:
        print(f"REFUSED: {e}", file=sys.stderr)
        return 2

    for line in lines:
        print(line)
    if args.gate:
        return 0 if accept else 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
