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
import sys
from pathlib import Path

RECALL_K = (5, 10)
GATE_K = 10

MIN_R10_DELTA = 0.02
MIN_MRR_DELTA = -0.005
MAX_MISS_DELTA = 0.005
MIN_CLUSTER_R10_DELTA = -0.02
MIN_CLUSTERS_FOR_INTERVAL = 5
MIN_CLUSTER_SIZE_FOR_VETO = 5
PROVENANCE_KEYS = ("corpus", "corpus_sha256", "split", "salt", "limit", "head")
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
    try:
        return int(value)
    except (TypeError, ValueError):
        raise Refused(f"{where}: meta.search_failures is not an integer ({value!r})")


# ---------------------------------------------------------------------------
# Held-out split
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Loading
# ---------------------------------------------------------------------------

def load_records(paths: list[Path], *, allow_partial: bool) -> tuple[dict, dict[str, dict]]:
    """Concatenate result files into (meta, id-keyed records).

    Accepts the `{"meta", "complete", "records"}` envelope or a bare list.
    Meta values from several files are merged; a key that differs across the
    files of one arm is recorded as a `MultiValue` (sorted by canonical JSON,
    originals kept) so the cross-arm check reports it.
    """
    out: dict[str, dict] = {}
    meta: dict = {}
    for path in paths:
        try:
            data = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError) as e:
            raise Refused(f"cannot read {path}: {e}")
        file_meta: dict = {}
        if isinstance(data, dict):
            if "records" in data:
                if data.get("complete") is False and not allow_partial:
                    raise Refused(
                        f"{path}: run did not finish (complete: false); "
                        "pass --allow-partial to compare anyway"
                    )
                file_meta = data.get("meta") or {}
                data = data["records"]
            else:
                data = data.get("cases", data.get("results"))
        if not isinstance(data, list):
            raise Refused(f"{path}: expected a JSON list of case records or a results envelope")
        for i, rec in enumerate(data):
            if not isinstance(rec, dict) or rec.get("id") is None:
                raise Refused(f"{path}: record #{i} has no `id`")
            cid = str(rec["id"])
            if cid in out:
                raise Refused(f"{path}: duplicate case id {cid!r} within one arm")
            out[cid] = rec
        for k, v in file_meta.items():
            if k not in meta:
                meta[k] = v
            elif k == "search_failures":
                meta[k] = parse_failure_count(meta[k], "merged meta") + parse_failure_count(v, str(path))
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
    diffs: list[str] = []
    for key in PROVENANCE_KEYS:
        b, c = base_meta.get(key), cand_meta.get(key)
        if b != c:
            diffs.append(f"{key}: baseline {b!r} vs candidate {c!r}")
    return diffs


# ---------------------------------------------------------------------------
# Per-case scoring
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Aggregation
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Verdict
# ---------------------------------------------------------------------------

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


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

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
) -> tuple[list[str], bool]:
    """Return (report lines, accept); raise Refused when no honest verdict exists."""
    ids = sorted(set(baseline) & set(candidate))
    n = len(ids)
    lines: list[str] = ["# CodeSage paired comparison", ""]
    lines.append(f"- Baseline: {fmt_meta(base_meta or {})}")
    lines.append(f"- Candidate: {fmt_meta(cand_meta or {})}")
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
        diffs = provenance_mismatch(base_meta, cand_meta)
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
