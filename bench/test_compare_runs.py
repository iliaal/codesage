#!/usr/bin/env python3
"""Regression tests for bench/compare-runs.py and the runner's split/results flags.

Run: python3 bench/test_compare_runs.py
"""
from __future__ import annotations

import contextlib
import hashlib as _hashlib
import importlib.machinery
import importlib.util
import io
import json
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
failures: list[str] = []


def _load(filename: str, modname: str):
    spec = importlib.util.spec_from_loader(
        modname, importlib.machinery.SourceFileLoader(modname, str(HERE / filename))
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def check(cond: bool, label: str) -> None:
    if not cond:
        failures.append(f"  {label}")


cmp = _load("compare-runs.py", "compare_runs")
runner = _load("codesage-bench-runner", "bench_runner")
ver = _load("_codesage_version.py", "codesage_version_helper")

_tmpdir = tempfile.TemporaryDirectory(prefix="compare-runs-")
tmp = Path(_tmpdir.name)


@contextlib.contextmanager
def patched(attr: str, fn):
    """Swap `runner.<attr>` for `fn`, restoring it afterwards."""
    original = getattr(runner, attr)
    setattr(runner, attr, fn)
    try:
        yield
    finally:
        setattr(runner, attr, original)


def patched_search(fn):
    return patched("run_codesage_search", fn)



def record(cid: str, cluster: str, expected: list[str], hits: list[str]) -> dict:
    rank = next((i for i, h in enumerate(hits, start=1) if h in set(expected)), None)
    return {
        "id": cid,
        "query": f"query {cid}",
        "expected_files": expected,
        "hits": hits,
        "first_hit_rank": rank,
        "source": f"{cluster}:{cid}",
        "repo": "/repo",
    }


def miss_hits() -> list[str]:
    return [f"noise/{i}.rs" for i in range(10)]


def hit_hits(target: str, at: int = 1) -> list[str]:
    hits = miss_hits()
    hits[at - 1] = target
    return hits


def write(name: str, records: list[dict], meta: dict | None = None,
          complete: bool = True) -> Path:
    p = tmp / name
    if meta is None:
        p.write_text(json.dumps(records))
    else:
        p.write_text(json.dumps({"meta": meta, "complete": complete, "records": records}))
    return p


def run(argv: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            rc = cmp.main(argv)
        except SystemExit as e:
            rc = int(e.code or 0)
    return rc, out.getvalue(), err.getvalue()


def sixty_cases() -> list[tuple[str, str]]:
    """60 ids across 6 clusters of 10."""
    return [(f"case-{c}-{i}", f"c{c}") for c in range(6) for i in range(10)]


def delta_row(out: str, metric: str) -> list[float]:
    for line in out.splitlines():
        if line.startswith(f"| {metric} | ") and line.count("|") == 6:
            return [float(x) for x in line.split("|")[2:6]]
    return []


def rejected_clauses(out: str) -> list[str]:
    lines = out.splitlines()
    if "REJECT" not in lines:
        return []
    return [l[2:] for l in lines[lines.index("REJECT") + 1:] if l.startswith("- ")]



same = [
    record(cid, cl, ["src/a.rs"], hit_hits("src/a.rs", at=(i % 5) + 1))
    for i, (cid, cl) in enumerate(sixty_cases())
]
a = write("same-a.json", same)
b = write("same-b.json", same)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200"])
check(rc == 0, f"identical: no --gate exits 0 (got {rc})")
check("Cluster key: case; clusters: 60" in out, "identical: default cluster key is case (60 clusters)")
check(delta_row(out, "recall@10") == [0.0, 0.0, 0.0, 0.0], "identical: r@10 delta row is all zeros")
check(delta_row(out, "MRR") == [0.0, 0.0, 0.0, 0.0], "identical: MRR delta row is all zeros")
check("\nREJECT\n" in out, "identical: verdict is REJECT")
clauses = rejected_clauses(out)
check(any(c.startswith("mean recall@10 delta +0.0000 <") for c in clauses), "identical: names the lift clause")
check(any(c.startswith("bootstrap lower bound of recall@10 delta +0.0000 <= 0") for c in clauses),
      "identical: names the LB clause")
check("veto (n >= 5) is vacuous" in out, "identical: case key reports the per-cluster veto as vacuous")
pred = next((l for l in out.splitlines() if l.startswith("Predicate:")), "")
check("vacuous under --cluster-key case" in pred and "no cluster with n >=" not in pred,
      "identical: Predicate line does not advertise the per-cluster clause under case")
rc_gate, _, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200", "--gate"])
check(rc_gate == 1, f"identical: --gate exits 1 on REJECT (got {rc_gate})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200",
                  "--cluster-key", "source-prefix"])
check("Cluster key: source-prefix; clusters: 6" in out, "identical: source-prefix key groups into 6")



base_recs, cand_recs = [], []
for i, (cid, cl) in enumerate(sixty_cases()):
    base_recs.append(record(cid, cl, ["src/t.rs"], miss_hits()))
    lifted = (i % 10) < 7 if int(cl[1]) < 4 else (i % 10) < 6  # 4*7 + 2*6 = 40
    cand_recs.append(record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs") if lifted else miss_hits()))
check(sum(1 for r in cand_recs if r["first_hit_rank"]) == 40, "spread: exactly 40 lifted cases")
a = write("spread-a.json", base_recs)
b = write("spread-b.json", cand_recs)
for key in ("case", "source-prefix"):
    rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate", "--cluster-key", key])
    check(rc == 0, f"spread/{key}: --gate exits 0 on ACCEPT (got {rc})")
    check("\nACCEPT\n" in out, f"spread/{key}: verdict is ACCEPT")
    row = delta_row(out, "recall@10")
    check(bool(row) and row[0] == 0.6667, f"spread/{key}: mean r@10 delta is +0.6667 (got {row})")
    check(bool(row) and row[2] > 0, f"spread/{key}: bootstrap lower bound > 0 (got {row})")
    check("| miss rate | 1.0000 | 0.3333 | -0.6667 |" in out, f"spread/{key}: miss-rate row")
    check("| median first-hit | MISS | 1 | |" in out, f"spread/{key}: median first-hit row")



base_recs, cand_recs = [], []
for i, (cid, cl) in enumerate(sixty_cases()):
    if cl == "c0":
        base_recs.append(record(cid, cl, ["src/t.rs"], miss_hits()))
        cand_recs.append(record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs")))
    elif cl == "c1":
        both = ["src/x.rs", "src/y.rs"] + miss_hits()[:8]
        base_recs.append(record(cid, cl, ["src/x.rs", "src/y.rs"], both))
        if i % 10 == 0:
            one = ["src/x.rs"] + miss_hits()[:9]  # recall@10 1.0 -> 0.5 on one case
            cand_recs.append(record(cid, cl, ["src/x.rs", "src/y.rs"], one))
        else:
            cand_recs.append(record(cid, cl, ["src/x.rs", "src/y.rs"], both))
    else:
        base_recs.append(record(cid, cl, ["src/t.rs"], miss_hits()))
        cand_recs.append(record(cid, cl, ["src/t.rs"], miss_hits()))
a = write("conc-a.json", base_recs)
b = write("conc-b.json", cand_recs)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate", "--cluster-key", "source-prefix"])
check(rc == 1, f"concentrated: --gate exits 1 (got {rc})")
check("\nREJECT\n" in out, "concentrated: verdict is REJECT")
check("| c0 | 10 | +1.0000 | yes |" in out, "concentrated: c0 cluster delta +1.0, veto-eligible")
check("| c1 | 10 | -0.0500 | yes |" in out, "concentrated: c1 cluster delta -0.05, veto-eligible")
clauses = rejected_clauses(out)
check(
    "per-cluster recall@10 delta < -0.0200 (clusters with n >= 5): c1 -0.0500" in clauses,
    "concentrated: names the per-cluster clause with the regressing cluster",
)
check(delta_row(out, "recall@10")[:1] == [0.1583], "concentrated: mean delta +0.1583 passes the lift clause")
check(not any(c.startswith("mean recall@10 delta") for c in clauses),
      "concentrated: lift clause is not listed as failing")



small = [record(f"s{i}", "c0", ["src/a.rs"], hit_hits("src/a.rs")) for i in range(20)]
a = write("small-a.json", small)
b = write("small-b.json", small)
rc, out, err = run(["--baseline", str(a), "--candidate", str(b)])
check(rc == 2, f"min-n: n=20 exits 2 (got {rc})")
check("REFUSED: paired n=20 is below --min-n 30" in err, "min-n: prints paired n and the floor")
check("Verdict" not in out, "min-n: no verdict printed when refused")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--min-n", "20", "--bootstrap", "200"])
check(rc == 0, f"min-n: --min-n 20 admits n=20 (got {rc})")



ids = [f"case-{i:04d}" for i in range(1000)]
first = [cmp.split_of(i, "salt-1") for i in ids]
second = [cmp.split_of(i, "salt-1") for i in ids]
check(first == second, "split: deterministic for the same salt")
train_share = first.count("train") / len(ids)
check(0.40 <= train_share <= 0.60, f"split: train share within 40-60% (got {train_share:.3f})")
other = [cmp.split_of(i, "salt-2") for i in ids]
check(other != first, "split: a different salt yields a different assignment")
check(
    all(runner.split_of(i, "salt-1") == s for i, s in zip(ids, first)),
    "split: codesage-bench-runner.split_of matches compare-runs.split_of",
)
check(cmp.split_of("x", "s") in ("train", "heldout"), "split: returns a known label")

corpus_yaml = tmp / "corpus.yaml"
corpus_yaml.write_text(
    "project_root: /repo\ncases:\n"
    + "".join(f"  - id: {i}\n    query: q\n    expected_files: [a]\n" for i in ids[:200])
)
rc, out, _ = run(["--split-report", str(corpus_yaml), "--salt", "salt-1"])
check(rc == 0, f"split-report: exits 0 (got {rc})")
n_train = first[:200].count("train")
check(f"- train: {n_train} of 200" in out, "split-report: train count matches split_of")
check(f"- heldout: {200 - n_train} of 200" in out, "split-report: heldout count matches split_of")

bad_yaml = tmp / "bad.yaml"
bad_yaml.write_text("cases: [unclosed\n")
rc, out, err = run(["--split-report", str(bad_yaml), "--salt", "s"])
check(rc == 2 and "REFUSED" in err and "malformed YAML" in err, f"split-report: malformed YAML exits 2 (rc={rc})")
no_cases = tmp / "nocases.yaml"
no_cases.write_text("project_root: /repo\n")
rc, out, err = run(["--split-report", str(no_cases), "--salt", "s"])
check(rc == 2 and "`cases` list" in err, f"split-report: missing cases exits 2 (rc={rc})")
rc, _, _ = run(["--split-report", str(corpus_yaml)])
check(rc == 2, f"split-report: missing --salt is a usage error (rc={rc})")
rc, _, _ = run(["--baseline", str(a), "--candidate", str(b), "--salt", "s"])
check(rc == 2, f"usage: --salt without --split-report is a usage error (rc={rc})")



a = write("seed-a.json", [record(cid, cl, ["src/t.rs"], miss_hits()) for cid, cl in sixty_cases()])
b = write(
    "seed-b.json",
    [
        # per-cluster lift counts 2..7 so the cluster means differ and the
        # resampled interval actually depends on which clusters are drawn
        record(
            cid, cl, ["src/t.rs"],
            hit_hits("src/t.rs", at=(i % 3) + 1) if (i % 10) < int(cl[1]) + 2 else miss_hits(),
        )
        for i, (cid, cl) in enumerate(sixty_cases())
    ],
)
for key in ("case", "source-prefix"):
    common = ["--baseline", str(a), "--candidate", str(b), "--bootstrap", "2000", "--cluster-key", key]
    _, out1, _ = run([*common, "--seed", "7"])
    _, out2, _ = run([*common, "--seed", "7"])
    check(out1 == out2, f"seed/{key}: identical seed reproduces byte-identical output")
    _, out3, _ = run([*common, "--seed", "8"])
    check(delta_row(out1, "recall@10")[1] != delta_row(out3, "recall@10")[1],
          f"seed/{key}: a different seed moves the bootstrap mean")
# With 6 unequal clusters the percentiles themselves are seed-sensitive; under
# the case key the 60 binary deltas make the resampled mean discrete (k/60),
# so two seeds can legitimately share a 2.5th percentile there.
_, out_s7, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "2000",
                    "--cluster-key", "source-prefix", "--seed", "7"])
_, out_s8, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "2000",
                    "--cluster-key", "source-prefix", "--seed", "8"])
check(delta_row(out_s7, "recall@10")[2:] != delta_row(out_s8, "recall@10")[2:],
      "seed/source-prefix: a different seed moves the percentiles")
boot7 = cmp.clustered_bootstrap({"a": [0.0, 1.0], "b": [1.0], "c": [0.0], "d": [1.0, 1.0], "e": [0.0]}, 500, 7)
boot7b = cmp.clustered_bootstrap({"a": [0.0, 1.0], "b": [1.0], "c": [0.0], "d": [1.0, 1.0], "e": [0.0]}, 500, 7)
check(boot7 == boot7b, "bootstrap: same seed gives identical mean/lb/ub")



check(cmp.percentile(list(range(10000)), 0.025) == 249, "percentile: 2.5% of 0..9999 is 249")
check(cmp.percentile(list(range(10000)), 0.975) == 9749, "percentile: 97.5% of 0..9999 is 9749")
check(cmp.percentile([5.0], 0.025) == 5.0 and cmp.percentile([5.0], 0.975) == 5.0, "percentile: n=1")
check(cmp.percentile([], 0.5) == 0.0, "percentile: empty")



two = [record(f"t{i}", "c0" if i < 20 else "c1", ["src/t.rs"], miss_hits()) for i in range(40)]
two_c = [record(f"t{i}", "c0" if i < 20 else "c1", ["src/t.rs"], hit_hits("src/t.rs")) for i in range(40)]
a = write("two-a.json", two)
b = write("two-b.json", two_c)
rc, out, err = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200",
                    "--cluster-key", "source-prefix"])
check(rc == 2, f"clusters: 2 source clusters refuses even without --gate (rc={rc})")
check("yields 2 cluster(s) (c0, c1); at least 5" in err, "clusters: refusal names the count and clusters")
check("Verdict" not in out and "2.5%" not in out, "clusters: no interval or verdict printed when refused")
rc, out, err = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200",
                    "--cluster-key", "repo"])
check(rc == 2 and "yields 1 cluster(s) (/repo)" in err, f"clusters: single repo refuses (rc={rc})")
check("Use --cluster-key case" in err, "clusters: non-case refusal suggests --cluster-key case")
rc, out, err = run(["--baseline", str(write("four-a.json", two[:4])), "--candidate",
                    str(write("four-b.json", two_c[:4])), "--bootstrap", "200", "--min-n", "3"])
check(rc == 2 and "--cluster-key case yields 4 cluster(s)" in err and "Use --cluster-key case" not in err
      and "--min-n" in err, f"clusters: case-key refusal names --min-n, not --cluster-key case (rc={rc}, err={err.strip()[-120:]})")

uneq = {"big": [1.0] * 20, "s1": [0.0], "s2": [0.0], "s3": [0.0], "s4": [0.0]}
check(cmp.pooled(uneq) == 20 / 24, "pooled: ratio of sums on unequal clusters is 20/24, not the mean of means")
uneq_a, uneq_b = [], []
for name, deltas in uneq.items():
    for j, d in enumerate(deltas):
        cid = f"u-{name}-{j}"
        uneq_a.append(record(cid, name, ["src/t.rs"], miss_hits()))
        uneq_b.append(record(cid, name, ["src/t.rs"], hit_hits("src/t.rs") if d else miss_hits()))
for _ in range(6):  # pad to n >= 30 with a sixth unchanged cluster
    j = len(uneq_a)
    uneq_a.append(record(f"u-pad-{j}", "pad", ["src/t.rs"], miss_hits()))
    uneq_b.append(record(f"u-pad-{j}", "pad", ["src/t.rs"], miss_hits()))
rc, out, _ = run(["--baseline", str(write("uneq-a.json", uneq_a)), "--candidate", str(write("uneq-b.json", uneq_b)),
                  "--bootstrap", "200", "--cluster-key", "source-prefix"])
row = delta_row(out, "recall@10")
check(rc == 0 and bool(row) and row[0] == round(20 / 30, 4),
      f"pooled: printed point estimate equals pooled() on the original sample (got {row})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200", "--gate"])
check(rc == 0 and "\nACCEPT\n" in out, f"clusters: same data under case key gives a verdict (rc={rc})")

fluke_a = [record(cid, "session", ["src/t.rs"], miss_hits()) for cid, _ in sixty_cases()]
fluke_b = [record(cid, "session", ["src/t.rs"], hit_hits("src/t.rs") if i < 2 else miss_hits())
           for i, (cid, _) in enumerate(sixty_cases())]
a = write("fluke-a.json", fluke_a)
b = write("fluke-b.json", fluke_b)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate"])
check(rc == 1 and "\nREJECT\n" in out, f"fluke: 2-of-60 lift on a single-source corpus is REJECT (rc={rc})")
check(any(c.startswith("bootstrap lower bound") for c in rejected_clauses(out)),
      "fluke: rejected on the lower-bound clause")

veto_a, veto_b = [], []
for c in range(5):
    for i in range(11):
        cid = f"v{c}-{i}"
        veto_a.append(record(cid, f"c{c}", ["src/t.rs"], miss_hits()))
        veto_b.append(record(cid, f"c{c}", ["src/t.rs"], hit_hits("src/t.rs")))
veto_a.append(record("m0", "manual", ["src/m.rs"], hit_hits("src/m.rs")))
veto_b.append(record("m0", "manual", ["src/m.rs"], miss_hits()))
a = write("veto-a.json", veto_a)
b = write("veto-b.json", veto_b)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate", "--cluster-key", "source-prefix"])
check(rc == 0 and "\nACCEPT\n" in out, f"veto: singleton regressing cluster cannot veto 55 lifts (rc={rc})")
check("| manual | 1 | -1.0000 | no (n < 5, informational) |" in out, "veto: singleton listed as informational")



mrr_a, mrr_b = [], []
for i, (cid, cl) in enumerate(sixty_cases()):
    if i < 50:
        mrr_a.append(record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs", at=1)))
        mrr_b.append(record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs", at=2)))
    else:
        mrr_a.append(record(cid, cl, ["src/t.rs"], miss_hits()))
        mrr_b.append(record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs", at=10)))
a = write("mrr-a.json", mrr_a)
b = write("mrr-b.json", mrr_b)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate"])
clauses = rejected_clauses(out)
check(rc == 1, f"mrr: --gate exits 1 (rc={rc})")
check("mean MRR delta -0.4000 < -0.0050" in clauses, f"mrr: MRR clause named (got {clauses})")
check(len(clauses) == 1, f"mrr: MRR is the only failing clause (got {clauses})")

miss_a, miss_b = [], []
for i, (cid, cl) in enumerate(sixty_cases()):
    if i < 55:
        miss_a.append(record(cid, cl, ["src/x.rs", "src/y.rs"], ["noise/0.rs", "src/x.rs"] + miss_hits()[2:]))
        miss_b.append(record(cid, cl, ["src/x.rs", "src/y.rs"], ["src/x.rs", "src/y.rs"] + miss_hits()[2:]))
    else:
        miss_a.append(record(cid, cl, ["src/x.rs", "src/y.rs"], ["src/x.rs"] + miss_hits()[1:]))
        miss_b.append(record(cid, cl, ["src/x.rs", "src/y.rs"], miss_hits()))
a = write("miss-a.json", miss_a)
b = write("miss-b.json", miss_b)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--gate"])
clauses = rejected_clauses(out)
check(rc == 1, f"miss: --gate exits 1 (rc={rc})")
check("miss-rate delta +0.0833 > +0.0050" in clauses, f"miss: miss-rate clause named (got {clauses})")
check(len(clauses) == 1, f"miss: miss-rate is the only failing clause (got {clauses})")



meta_base = {"corpus": "c.yaml", "corpus_sha256": "deadbeef", "split": "heldout", "salt": "s1",
             "limit": 10, "head": "abc", "search_failures": 0}
recs_a = [record(cid, cl, ["src/t.rs"], miss_hits()) for cid, cl in sixty_cases()]
recs_b = [record(cid, cl, ["src/t.rs"], hit_hits("src/t.rs")) for cid, cl in sixty_cases()]
a = write("env-a.json", recs_a, meta=meta_base)
b = write("env-b.json", recs_b, meta=meta_base)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200"])
check(rc == 0 and "\nACCEPT\n" in out, f"envelope: complete envelope compares (rc={rc})")
check("- Baseline: corpus='c.yaml', corpus_sha256='deadbeef', split='heldout', salt='s1', limit=10, "
      "head='abc', search_failures=0" in out, "envelope: provenance incl. limit/sha/failures printed per arm")
other_limit = write("env-limit.json", recs_b, meta={**meta_base, "limit": 50})
rc, out, err = run(["--baseline", str(a), "--candidate", str(other_limit), "--bootstrap", "200"])
check(rc == 2 and "limit: baseline 10 vs candidate 50" in err, f"mismatch: --limit differs refuses (rc={rc})")
other_sha = write("env-sha.json", recs_b, meta={**meta_base, "corpus_sha256": "cafe"})
rc, out, err = run(["--baseline", str(a), "--candidate", str(other_sha), "--bootstrap", "200"])
check(rc == 2 and "corpus_sha256: baseline 'deadbeef' vs candidate 'cafe'" in err,
      f"mismatch: same basename, different corpus bytes refuses (rc={rc})")
failed = write("env-failed.json", recs_b, meta={**meta_base, "search_failures": 3})
rc, out, err = run(["--baseline", str(a), "--candidate", str(failed), "--bootstrap", "200"])
check(rc == 2 and "search failures recorded (baseline 0, candidate 3)" in err,
      f"failures: refused with counts (rc={rc})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(failed), "--bootstrap", "200", "--allow-failures"])
check(rc == 0 and "search_failures=3" in out, f"failures: --allow-failures compares and shows the count (rc={rc})")
no_id = write("env-noid.json", recs_b[:5] + [{"query": "q", "hits": []}] + recs_b[6:], meta=meta_base)
rc, out, err = run(["--baseline", str(a), "--candidate", str(no_id), "--bootstrap", "200"])
check(rc == 2 and "env-noid.json: record #5 has no `id`" in err, f"no-id: exit 2 naming file and index (rc={rc})")
m1 = write("m1.json", recs_b[:20], meta={**meta_base, "head": "h1"})
m2 = write("m2.json", recs_b[20:40], meta={**meta_base, "head": "h2"})
m3 = write("m3.json", recs_b[40:], meta={**meta_base, "head": {"sha": "h3"}})
meta_fwd, _ = cmp.load_records([m1, m2, m3], allow_partial=False)
meta_rev, _ = cmp.load_records([m3, m2, m1], allow_partial=False)
check(meta_fwd == meta_rev, f"merge: three files in two orders give the same meta ({meta_fwd['head']} vs {meta_rev['head']})")
check(isinstance(meta_fwd["head"], cmp.MultiValue) and list(meta_fwd["head"]) == ["h1", "h2", {"sha": "h3"}],
      f"merge: differing values accumulate as a MultiValue, originals kept ({meta_fwd['head']!r})")
check(meta_fwd["head"] != ["h1", "h2", {"sha": "h3"}], "merge: MultiValue is never equal to a plain list")
check(repr(meta_fwd["head"]) == "multi['h1', 'h2', {'sha': 'h3'}]", "merge: MultiValue renders as multi[...]")
check(meta_fwd["search_failures"] == 0 and meta_fwd["limit"] == 10, "merge: equal keys stay scalar")
rc, out, err = run(["--baseline", str(a), "--candidate", str(m1), str(m2), str(m3), "--bootstrap", "200"])
check(rc == 2 and "head: baseline 'abc' vs candidate multi['h1', 'h2', {'sha': 'h3'}]" in err,
      f"head: differing HEAD refuses (rc={rc}, err={err.strip()[-160:]})")
same_head = write("env-samehead.json", recs_b, meta={**meta_base, "head": "def"})
rc, out, err = run(["--baseline", str(a), "--candidate", str(same_head), "--bootstrap", "200"])
check(rc == 2 and "head: baseline 'abc' vs candidate 'def'" in err, f"head: scalar HEAD mismatch refuses (rc={rc})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(m1), str(m2), str(m3), "--bootstrap", "200", "--allow-mismatch"])
check(rc == 0 and "\nACCEPT\n" in out, f"merge: concatenated candidate compares with --allow-mismatch (rc={rc})")
check("head=multi['h1', 'h2', {'sha': 'h3'}]" in out, "merge: multi-valued meta rendered as multi[...]")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(m3), str(m2), str(m1), "--bootstrap", "200", "--allow-mismatch"])
check(rc == 0 and "\nACCEPT\n" in out, f"merge: reversed order compares with --allow-mismatch (rc={rc})")
l1 = write("l1.json", recs_b[:30], meta={**meta_base, "features": ["cpu", "cuda"]})
l2 = write("l2.json", recs_b[30:], meta={**meta_base, "features": ["cpu"]})
lf, _ = cmp.load_records([l1, l2], allow_partial=False)
lr, _ = cmp.load_records([l2, l1], allow_partial=False)
check(lf == lr and isinstance(lf["features"], cmp.MultiValue) and list(lf["features"]) == [["cpu", "cuda"], ["cpu"]],
      f"merge: list-valued key accumulates as a MultiValue of lists in both orders ({lf['features']!r} / {lr['features']!r})")
l3 = write("l3.json", recs_b[:30], meta={**meta_base, "features": ["cpu", "cuda"]})
l4 = write("l4.json", recs_b[30:], meta={**meta_base, "features": ["cpu", "cuda"]})
lsame, _ = cmp.load_records([l3, l4], allow_partial=False)
check(lsame["features"] == ["cpu", "cuda"] and not isinstance(lsame["features"], cmp.MultiValue),
      f"merge: equal list values stay a plain list ({lsame['features']!r})")
bad_count = write("env-badcount.json", recs_b, meta={**meta_base, "search_failures": "many"})
rc, out, err = run(["--baseline", str(a), "--candidate", str(bad_count), "--bootstrap", "200"])
check(rc == 2 and "meta.search_failures is not an integer ('many')" in err, f"failures: non-integer count exits 2 (rc={rc})")
ghost = write("env-ghost.json", recs_b, meta={**meta_base, "search_failures": 3})
rc, out, err = run(["--baseline", str(a), "--candidate", str(ghost), "--bootstrap", "200", "--exclude-failed"])
check(rc == 2 and "search failures recorded (baseline 0, candidate 3)" in err and "cannot locate them" in err,
      f"ghost: meta-3/records-0 refused even with --exclude-failed (rc={rc}, err={err.strip()[-160:]})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(ghost), "--bootstrap", "200", "--exclude-failed", "--allow-failures"])
check(rc == 0 and "- Excluded failed ids (both arms): none" in out, f"ghost: --allow-failures compares, excluded line says none (rc={rc})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "200"])
check("- Excluded failed ids (both arms): none" in out, "excluded line printed unconditionally")
nulls = write("env-null.json", [{**r, "error": None} for r in recs_b], meta=meta_base)
rc, out, _ = run(["--baseline", str(a), "--candidate", str(nulls), "--bootstrap", "200"])
check(rc == 0 and "| search failures | 0 | 0 | |" in out, f"errors: error: null is not counted (rc={rc})")

err_a = [dict(r) for r in recs_a]
err_b = [dict(r) for r in recs_b]
err_a[3] = {**err_a[3], "error": "timeout", "hits": [], "first_hit_rank": None}
err_b[7] = {**err_b[7], "error": "rc=65", "hits": [], "first_hit_rank": None}
err_b[8] = {**err_b[8], "error": "rc=65", "hits": [], "first_hit_rank": None}
ea = write("err-a.json", err_a, meta=meta_base)          # meta says 0, records say 1
eb = write("err-b.json", err_b, meta={**meta_base, "search_failures": 2})
rc, out, err = run(["--baseline", str(ea), "--candidate", str(eb), "--bootstrap", "200"])
check(rc == 2 and "search failures recorded (baseline 1, candidate 2)" in err,
      f"errors: counts derive from records even when meta says 0 (rc={rc}, err={err.strip()[-120:]})")
rc, out, _ = run(["--baseline", str(ea), "--candidate", str(eb), "--bootstrap", "200", "--allow-failures"])
check(rc == 0 and "| search failures | 1 | 2 | |" in out, f"errors: --allow-failures shows per-arm counts in Arms (rc={rc})")
check("- baseline: case-0-3" in out and "- candidate: case-0-7, case-0-8" in out, "errors: failed ids listed per arm")
check("Paired cases: 60" in out, "errors: --allow-failures keeps all 60 paired")
rc, out, _ = run(["--baseline", str(ea), "--candidate", str(eb), "--bootstrap", "200", "--allow-failures", "--exclude-failed"])
check(rc == 0 and "Paired cases: 57" in out, f"errors: --exclude-failed pairs 60 minus the 3 failed ids (rc={rc})")
check("- Excluded failed ids (both arms): 3 (case-0-3, case-0-7, case-0-8)" in out, "errors: excluded ids named")
check("| search failures | 0 | 0 | |" in out, "errors: no failures remain after exclusion")
eb_over = write("err-b-over.json", err_b, meta={**meta_base, "search_failures": 5})  # 2 attributable, 3 ghosts
rc, out, err = run(["--baseline", str(ea), "--candidate", str(eb_over), "--bootstrap", "200", "--exclude-failed"])
check(rc == 2 and "search failures recorded (baseline 0, candidate 3)" in err,
      f"errors: exclusion subtracts only attributed failures, 3 remain (rc={rc}, err={err.strip()[-120:]})")
rc, out, _ = run(["--baseline", str(ea), "--candidate", str(eb), "--bootstrap", "200", "--exclude-failed"])
check(rc == 0 and "Paired cases: 57" in out, f"errors: --exclude-failed alone is sufficient (rc={rc})")

rc, _, err = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "199"])
check(rc == 2 and "--bootstrap must be >= 200" in err, f"bootstrap: 199 is a usage error (rc={rc})")
rc, _, err = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "500"])
check(rc == 0 and "WARNING: --bootstrap 500 is below 1000" in err, f"bootstrap: 500 warns (rc={rc})")
rc, _, err = run(["--baseline", str(a), "--candidate", str(b), "--bootstrap", "1000"])
check(rc == 0 and "WARNING" not in err, f"bootstrap: 1000 is silent (rc={rc})")
partial = write("env-partial.json", recs_b[:40], meta=meta_base, complete=False)
rc, out, err = run(["--baseline", str(a), "--candidate", str(partial), "--bootstrap", "200"])
check(rc == 2 and "complete: false" in err, f"partial: refused without --allow-partial (rc={rc})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(partial), "--bootstrap", "200", "--allow-partial"])
check(rc == 0 and "Paired cases: 40" in out, f"partial: --allow-partial compares the 40 paired (rc={rc})")
other_salt = write("env-b2.json", recs_b, meta={**meta_base, "salt": "s2"})
rc, out, err = run(["--baseline", str(a), "--candidate", str(other_salt), "--bootstrap", "200"])
check(rc == 2 and "salt: baseline 's1' vs candidate 's2'" in err, f"mismatch: salt differs refuses (rc={rc})")
rc, out, _ = run(["--baseline", str(a), "--candidate", str(other_salt), "--bootstrap", "200", "--allow-mismatch"])
check(rc == 0, f"mismatch: --allow-mismatch compares (rc={rc})")
legacy = write("env-legacy.json", recs_b)
rc, out, err = run(["--baseline", str(a), "--candidate", str(legacy), "--bootstrap", "200"])
check(rc == 2 and "corpus: baseline 'c.yaml' vs candidate None" in err,
      f"mismatch: envelope vs legacy list is a provenance mismatch (rc={rc})")
dup = write("dup.json", recs_a + recs_a[:1])
rc, _, err = run(["--baseline", str(dup), "--candidate", str(b)])
check(rc == 2 and "duplicate case id 'case-0-0'" in err, f"duplicate ids: exits 2 and names the id (rc={rc})")
bad_rank = write("bad-rank.json", [{**recs_b[0], "first_hit_rank": "one"}] + recs_b[1:])
rc, _, err = run(["--baseline", str(write("bad-rank-base.json", recs_a)), "--candidate", str(bad_rank), "--bootstrap", "200"])
check(rc == 2 and "record 'case-0-0': first_hit_rank must be a positive integer ('one')" in err,
      f"bad rank: exits 2 naming the record (rc={rc}, err={err.strip()[-100:]})")
for bad_value in (-1, True, 2.9, 0):
    bad_rank = write("bad-rank2.json", [{**recs_b[0], "first_hit_rank": bad_value}] + recs_b[1:])
    rc, _, err = run(["--baseline", str(write("bad-rank-base.json", recs_a)), "--candidate", str(bad_rank), "--bootstrap", "200"])
    check(rc == 2 and f"must be a positive integer ({bad_value!r})" in err,
          f"bad rank: {bad_value!r} is refused (rc={rc}, err={err.strip()[-100:]})")
ok_rank = write("ok-rank.json", [{**recs_b[0], "first_hit_rank": 1.0}, {**recs_b[1], "first_hit_rank": "1"}] + recs_b[2:])
rc, out, _ = run(["--baseline", str(write("bad-rank-base.json", recs_a)), "--candidate", str(ok_rank), "--bootstrap", "200"])
check(rc == 0 and "\nACCEPT\n" in out, f"bad rank: integral 1.0 and '1' are accepted (rc={rc})")



proj = tmp / "proj"
proj.mkdir()
run_corpus = tmp / "run-corpus.yaml"
run_ids = [f"r{i}" for i in range(20)]
run_corpus.write_text(
    f"project_root: {proj}\ncases:\n"
    + "".join(f"  - id: {i}\n    query: q {i}\n    expected_files: [a.rs]\n    source: session\n"
              for i in run_ids)
)
canned = json.dumps([{"file_path": "a.rs", "content": "fn a() {}"}, {"file_path": "b.rs", "content": ""}])


def canned_search(*_a, **_k):
    return canned, None


def run_runner(argv: list[str]) -> tuple[int, str, str]:
    out, err = io.StringIO(), io.StringIO()
    old = sys.argv
    sys.argv = ["codesage-bench-runner", *argv]
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                rc = runner.main()
            except SystemExit as e:
                # Mirror sys.exit(str) onto stderr because this harness catches SystemExit.
                if isinstance(e.code, int):
                    rc = e.code
                else:
                    print(e.code, file=sys.stderr)
                    rc = 1
    finally:
        sys.argv = old
    return rc, out.getvalue(), err.getvalue()


results_path = tmp / "nested" / "dir" / "results.json"
with patched_search(canned_search):
    rc, out, err = run_runner([str(run_corpus), "--results-json", str(results_path), "--split", "train", "--salt", "s"])
check(rc == 0, f"runner: split run exits 0 (rc={rc}, err={err[-200:]})")
env = json.loads(results_path.read_text())
check(env.get("complete") is True, "runner: finished run is marked complete")
train_ids = [i for i in run_ids if runner.split_of(i, "s") == "train"]
check([r["id"] for r in env["records"]] == train_ids, "runner: results hold exactly the train ids in corpus order")
check(all(r["first_hit_rank"] == 1 and r["hits"] == ["a.rs", "b.rs"] and "error" not in r for r in env["records"]),
      "runner: canned hits scored at rank 1 with no error field")
meta = env["meta"]
check(meta.get("split") == "train" and meta.get("salt") == "s" and meta.get("corpus") == "run-corpus.yaml",
      f"runner: meta carries corpus/split/salt (got {meta})")
check(meta.get("corpus_sha256") == _hashlib.sha256(run_corpus.read_bytes()).hexdigest(),
      "runner: meta.corpus_sha256 is the sha256 of the corpus bytes")
check(meta.get("limit") == 10 and meta.get("search_failures") == 0, f"runner: meta carries limit and search_failures (got {meta})")
check("head" in meta and "model" in meta and "reranker" in meta, "runner: meta carries head/model/reranker")
check("split=train salt=s" in out and "<!-- METRICS:" in out, "runner: METRICS gains split= and salt=")
check(f"cases={len(train_ids)}" in out, "runner: METRICS case count is the split's")
check(f"- **Corpus**: `run-corpus.yaml` — {len(train_ids)} cases (split train, salt s), top-10" in out,
      "runner: scorecard header names the split and salt")
check("ground-truth cases (split train, salt s) on `proj`" in out, "runner: quotable one-liner names the split and salt")
with patched_search(canned_search):
    rc, _, err = run_runner([str(run_corpus), "--split", "train"])
    check(rc == 2 and "given together" in err, f"runner: --split without --salt exits 2 (rc={rc})")
    rc, _, err = run_runner([str(run_corpus), "--split", "train", "--salt", "bad salt -->"])
    check(rc == 2 and "[A-Za-z0-9_.-]+" in err, f"runner: salt outside the allowed alphabet exits 2 (rc={rc})")
    rc, out, err = run_runner([str(run_corpus), "--results-json", str(tmp / "full.json")])
env = json.loads((tmp / "full.json").read_text())
check(rc == 0 and len(env["records"]) == 20 and env["meta"]["split"] is None,
      "runner: unsplit run writes all 20 records with split null")
check("split=" not in out and "(split" not in out, "runner: no split text when no split ran")

def flaky_search(_bin, _root, query, _limit):
    if query == "q r3":
        return "", "timeout"
    if query == "q r7":
        return "", "rc=1"
    return canned, None


with patched_search(flaky_search):
    rc, out, err = run_runner([str(run_corpus), "--results-json", str(tmp / "flaky.json")])
env = json.loads((tmp / "flaky.json").read_text())
check(rc == 3 and env["complete"] is True, f"runner: flaky run completes its JSON but exits 3 (rc={rc})")
check(env["meta"]["search_failures"] == 2, f"runner: meta.search_failures counts both failures (got {env['meta'].get('search_failures')})")
errs = {r["id"]: r.get("error") for r in env["records"] if "error" in r}
check(errs == {"r3": "timeout", "r7": "rc=1"}, f"runner: per-record error tags (got {errs})")
check(all(r["first_hit_rank"] is None for r in env["records"] if "error" in r), "runner: failed searches scored as misses")
rc, out, err = run(["--baseline", str(tmp / "full.json"), "--candidate", str(tmp / "flaky.json"), "--min-n", "20",
                    "--bootstrap", "200"])
check(rc == 2 and "search failures recorded (baseline 0, candidate 2)" in err,
      f"runner->compare: flaky arm is refused end to end (rc={rc})")

BANNER = (
    "codesage 0.26.1 (release)\n"
    "  target: x86_64-unknown-linux\n"
    "  features compiled: cpu, cuda\n"
    "  device configured: gpu\n"
)
info = ver.parse_version_banner(BANNER)
check(info == {"version": "0.26.1 (release)", "version_token": "0.26.1", "build": "release",
               "build_target": "x86_64-unknown-linux", "features": "cpu, cuda", "device": "gpu"},
      f"banner: parsed fields (got {info})")
check(ver.parse_version_banner("codesage 0.4.0\n") == {"version": "0.4.0", "version_token": "0.4.0"},
      "banner: one-line legacy form has no build token")
check(ver.parse_version_banner("") == {"version": "unknown", "version_token": "unknown"}, "banner: empty output is unknown")
check(not hasattr(runner, "parse_version_banner"), "runner: does not re-export parse_version_banner")


with patched("codesage_version_info", lambda _bin, cwd=None: ver.parse_version_banner(BANNER)), patched_search(canned_search):
    rc, out, err = run_runner([str(run_corpus), "--results-json", str(tmp / "banner.json")])
check(rc == 0, f"banner: run exits 0 (rc={rc})")
check(not hasattr(runner, "codesage_version"), "banner: dead codesage_version removed from the runner")
check(ver.parse_version_banner(BANNER)["version"] == "0.26.1 (release)", "banner: version keeps the build suffix")
metrics_lines = [l for l in out.splitlines() if "METRICS:" in l]
check(len(metrics_lines) == 1 and metrics_lines[0].startswith("<!-- METRICS: ") and metrics_lines[0].endswith(" -->"),
      f"banner: METRICS comment is a single line (got {metrics_lines})")
check(" codesage=0.26.1 build=release " in metrics_lines[0] and "target:" not in metrics_lines[0]
      and "(release)" not in metrics_lines[0], "banner: METRICS names the binary as two whitespace-free tokens")
check(all(" " not in tok.split("=", 1)[1] for tok in metrics_lines[0][len("<!-- METRICS: "):-len(" -->")].split()
          if tok.count("=") == 1 and not tok.startswith("baseline")),
      "banner: every METRICS value except the free-text baseline is whitespace-free")
quotable = out.split("## Quotable one-liner", 1)[1].split("<!-- METRICS:", 1)[0].strip().splitlines()
check(len(quotable) == 1 and quotable[0].startswith("> CodeSage 0.26.1 (release) hits "),
      f"banner: quotable one-liner is a single line (got {quotable})")
check("target:" not in out and "features compiled" not in out, "banner: no banner fields leak into the scorecard")
check("- **CodeSage**: 0.26.1 (release)" in out, "banner: header CodeSage line is the first banner line only")
bmeta = json.loads((tmp / "banner.json").read_text())["meta"]
check(bmeta["codesage"] == "0.26.1 (release)" and bmeta["build_target"] == "x86_64-unknown-linux"
      and bmeta["features"] == "cpu, cuda" and bmeta["device"] == "gpu",
      f"banner: meta carries build_target/features/device (got {bmeta})")

atr = _load("agent-task-runner", "agent_task_runner_under_test")
_orig_atr_banner = atr.run_version_banner
atr.run_version_banner = lambda _bin, timeout=10: BANNER
try:
    check(atr.codesage_version("codesage") == "0.26.1 (release)", "agent-task-runner: codesage_version is the first banner line")
finally:
    atr.run_version_banner = _orig_atr_banner
check(not hasattr(runner, "parse_result_paths"), "runner: dead parse_result_paths removed")

five_corpus = tmp / "five.yaml"
five_corpus.write_text(
    f"project_root: {proj}\ncases:\n"
    + "".join(f"  - id: f{i}\n    query: q f{i}\n    expected_files: [a.rs]\n" for i in range(5))
)
with patched_search(lambda *_a, **_k: ("", "rc=65")):
    rc, out, err = run_runner([str(five_corpus), "--results-json", str(tmp / "allfail.json")])
check(rc == 3, f"failures: all-failed run exits 3 (rc={rc})")
m_line = next((l for l in out.splitlines() if l.startswith("<!-- METRICS:")), "")
check(" search_failures=5 -->" in m_line, f"failures: METRICS carries search_failures=5 (got {m_line[-60:]})")
check("> [INVALID: 5 search failures] CodeSage " in out, "failures: one-liner prefixed INVALID")
check("- **Search failures:** 5 (scored as misses)" in out, "failures: aggregate bullet present")
check("5 of 5 searches failed" in err, "failures: stderr explains the exit code")
env = json.loads((tmp / "allfail.json").read_text())
check(env["complete"] is True and env["meta"]["search_failures"] == 5, "failures: results JSON still written complete")
with patched_search(lambda *_a, **_k: ("", "rc=65")):
    rc, out, _ = run_runner([str(five_corpus), "--allow-search-failures"])
check(rc == 0 and "[INVALID: 5 search failures]" in out, f"failures: --allow-search-failures exits 0, still flagged (rc={rc})")
with patched_search(canned_search):
    rc, out, _ = run_runner([str(five_corpus)])
check(rc == 0 and " search_failures=0 -->" in out and "INVALID" not in out and "Search failures" not in out,
      "failures: clean run reports search_failures=0 and no flags")

bad_corpus = tmp / "bad-corpus.yaml"
bad_corpus.write_text("cases: [unclosed\n")
with patched_search(canned_search):
    rc, _, err = run_runner([str(bad_corpus), "--results-json", str(tmp / "never.json")])
check(rc == 2 and "malformed YAML" in err, f"corpus: malformed YAML exits 2 (rc={rc})")
check(not (tmp / "never.json").exists(), "corpus: no envelope written for a malformed corpus")
with patched_search(canned_search):
    rc, _, err = run_runner([str(bad_corpus), "--results-json", str(tmp / "never-dir" / "deep" / "r.json")])
check(rc == 2 and not (tmp / "never-dir").exists(), f"corpus: rejected corpus creates no --results-json directories (rc={rc})")
no_root = tmp / "no-root.yaml"
no_root.write_text("cases: []\n")
with patched_search(canned_search):
    rc, _, err = run_runner([str(no_root)])
check(rc == 2 and "`project_root`" in err, f"corpus: missing project_root exits 2 (rc={rc})")
no_cases_c = tmp / "no-cases.yaml"
no_cases_c.write_text(f"project_root: {proj}\n")
with patched_search(canned_search):
    rc, _, err = run_runner([str(no_cases_c)])
check(rc == 2 and "`cases` list" in err, f"corpus: missing cases exits 2 (rc={rc})")

# `codesage --version` runs in the project root, so `device` is the project's
stub_bin = tmp / "codesage-stub"
stub_bin.write_text('#!/bin/sh\necho "codesage 9.9.9 (stub)"\necho "  device configured: $PWD"\n')
stub_bin.chmod(0o755)
with patched_search(canned_search):
    rc, out, _ = run_runner([str(run_corpus), "--codesage-bin", str(stub_bin), "--results-json", str(tmp / "cwd.json")])
cwd_meta = json.loads((tmp / "cwd.json").read_text())["meta"]
check(rc == 0 and cwd_meta["codesage"] == "9.9.9 (stub)", f"version cwd: stub banner parsed (rc={rc}, got {cwd_meta.get('codesage')})")
check(Path(cwd_meta["device"]).resolve() == proj.resolve(),
      f"version cwd: device line reflects the project root, not the runner cwd (got {cwd_meta.get('device')})")

def bad_case_corpus(name: str, case_yaml: str) -> Path:
    p = tmp / name
    p.write_text(f"project_root: {proj}\ncases:\n  - id: ok\n    query: q\n    expected_files: [a.rs]\n{case_yaml}")
    return p


for label, case_yaml, expect in (
    ("non-mapping case", "  - just-a-string\n", "case #1 is not a mapping"),
    ("numeric id", "  - id: 7\n    query: q\n    expected_files: [a.rs]\n", "case #1 needs a string `id`"),
    ("missing query", "  - id: x\n    expected_files: [a.rs]\n", "case #1 needs a string `query`"),
    ("string expected_files", "  - id: x\n    query: q\n    expected_files: a.rs\n", "case #1 needs `expected_files`"),
    ("empty expected_files", "  - id: x\n    query: q\n    expected_files: []\n", "case #1 needs `expected_files`"),
    ("non-string path", "  - id: x\n    query: q\n    expected_files: [1]\n", "case #1 needs `expected_files`"),
):
    with patched_search(canned_search):
        rc, _, err = run_runner([str(bad_case_corpus(f"case-{label.replace(' ', '-')}.yaml", case_yaml)),
                                 "--results-json", str(tmp / "never2.json")])
    check(rc == 2 and expect in err, f"corpus: {label} exits 2 naming the case (rc={rc}, err={err.strip()[-80:]})")
check(not (tmp / "never2.json").exists(), "corpus: no envelope written for an invalid case")

abl = _load("ablation.py", "ablation_under_test")
stub_runner = tmp / "stub-runner.py"
stub_runner.write_text(
    "import os, sys\n"
    "if 'CODESAGE_DEFINITION_BOOST' in os.environ:\n"
    "    print('<!-- METRICS: miss_rate=0.2000 median_first=1 r5=0.8000 r10=0.8000 cases=5 search_failures=0 -->')\n"
    "    sys.exit(0)\n"
    "print('<!-- METRICS: miss_rate=1.0000 median_first=MISS r5=0.0000 r10=0.0000 cases=5 search_failures=2 -->')\n"
    "sys.exit(3)\n"
)
abl_out, abl_err = io.StringIO(), io.StringIO()
old_argv = sys.argv
sys.argv = ["ablation.py", str(run_corpus), "--runner", str(stub_runner), "--arms", "no_definition_boost"]
try:
    with contextlib.redirect_stdout(abl_out), contextlib.redirect_stderr(abl_err):
        abl_rc = abl.main()
finally:
    sys.argv = old_argv
abl_text = abl_out.getvalue()
check(abl_rc != 0, f"ablation: invalid arm makes main() return nonzero (rc={abl_rc})")
check("| baseline | INVALID (2 search failures) | — | — | — | — | — |" in abl_text, "ablation: invalid baseline row rendered INVALID")
ndb_row = next((l for l in abl_text.splitlines() if l.startswith("| no_definition_boost")), "")
check(ndb_row.endswith("| 0.8000 | — |  |") and "=base" not in ndb_row,
      f"ablation: valid arm has no =base and no delta against an invalid baseline (got {ndb_row!r})")
check("## Invalid runs" in abl_text and f"- `{run_corpus.name}/baseline`" in abl_text, "ablation: invalid runs section lists the arm")
check("search_failures" in abl.METRIC_KEYS, "ablation: search_failures is a tracked metric key")
check(abl.invalid_reason(0, {"search_failures": "0"}) is None
      and abl.invalid_reason(3, {}) == "INVALID (runner rc=3, no METRICS)"
      and abl.invalid_reason(1, {}) == "INVALID (runner rc=1, no METRICS)"
      and abl.invalid_reason(1, {"r10": "0.5"}) == "INVALID (runner rc=1)"
      and abl.invalid_reason(3, {"search_failures": "0"}) == "INVALID (? search failures)"
      and abl.invalid_reason(0, {"search_failures": "4"}) == "INVALID (4 search failures)", "ablation: invalid_reason rules")
check(abl.metrics_signature({}) == () and abl.metrics_signature({abl.INVALID_KEY: "x", "r10": "1"}) == (),
      "ablation: empty and invalid metrics both have an empty signature")


def run_ablation(argv: list[str]) -> tuple[int, str]:
    out = io.StringIO()
    old = sys.argv
    sys.argv = ["ablation.py", *argv]
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            rc = abl.main()
    finally:
        sys.argv = old
    return rc, out.getvalue()


crash_runner = tmp / "crash-runner.py"
crash_runner.write_text(
    "import os, sys\n"
    "if 'CODESAGE_RRF_K' in os.environ:\n"
    "    sys.exit(1)\n"
    "print('<!-- METRICS: miss_rate=0.2000 median_first=1 r5=0.8000 r10=0.8000 cases=5 search_failures=0 -->')\n"
    "sys.exit(0)\n"
)
rc, text = run_ablation([str(run_corpus), "--runner", str(crash_runner), "--arms", "rrf_k_30"])
check(rc == 1, f"ablation: crashed arm makes main() return 1 (rc={rc})")
check("| rrf_k_30 | INVALID (runner rc=1, no METRICS) | — | — | — | — | — |" in text, "ablation: crashed arm row is INVALID")
check(f"- `{run_corpus.name}/rrf_k_30`" in text, "ablation: crashed arm listed under Invalid runs")
with contextlib.redirect_stderr(io.StringIO()):
    _, differed, compared_counts, invalid_list = abl.run_sweep(
        [run_corpus], ["baseline", "rrf_k_30"], crash_runner, "codesage", 10
    )
check("rrf_k_30" not in differed and compared_counts["rrf_k_30"] == 0 and invalid_list == [f"{run_corpus.name}/rrf_k_30"],
      f"ablation: crashed arm is neither differed nor compared ({differed}, {compared_counts}, {invalid_list})")
check("## No measurable effect" not in text, "ablation: crashed needs-patch arm is not reported inert")

split_runner = tmp / "split-runner.py"
split_runner.write_text(
    "import os, sys\n"
    "corpus = os.path.basename(sys.argv[1])\n"
    "if corpus.startswith('c1') and 'CODESAGE_RRF_K' not in os.environ:\n"
    "    sys.exit(1)\n"
    "print('<!-- METRICS: miss_rate=0.2000 median_first=1 r5=0.8000 r10=0.8000 cases=5 search_failures=0 -->')\n"
    "sys.exit(0)\n"
)
c1, c2 = tmp / "c1.yaml", tmp / "c2.yaml"
rc, text = run_ablation([str(c1), str(c2), "--runner", str(split_runner), "--arms", "rrf_k_30"])
with contextlib.redirect_stderr(io.StringIO()):
    _, differed, compared_counts, _ = abl.run_sweep([c1, c2], ["baseline", "rrf_k_30"], split_runner, "codesage", 10)
check(compared_counts["rrf_k_30"] == 1 and "rrf_k_30" not in differed, f"ablation: compared once, never differed ({compared_counts})")
check("## No measurable effect" not in text and "| baseline | INVALID (runner rc=1, no METRICS)" in text,
      "ablation: baseline invalid on c1 + identical on c2 is NOT reported inert")
check(rc == 1, f"ablation: invalid baseline on one corpus fails the run (rc={rc})")
identical_runner = tmp / "identical-runner.py"
identical_runner.write_text(
    "print('<!-- METRICS: miss_rate=0.2000 median_first=1 r5=0.8000 r10=0.8000 cases=5 search_failures=0 -->')\n"
)
rc, text = run_ablation([str(c1), str(c2), "--runner", str(identical_runner), "--arms", "rrf_k_30"])
check(rc == 0 and "## No measurable effect" in text and "- `rrf_k_30`" in text and "rrf_k_30  =base" in text,
      f"ablation: validly compared and identical on every corpus IS reported inert (rc={rc})")
rc, text = run_ablation([str(c1), str(c2), "--runner", str(identical_runner), "--arms", "rrf_k_30,rrf_k_30,baseline"])
check(rc == 0 and "## No measurable effect" in text and "- `rrf_k_30`" in text,
      f"ablation: duplicated arm still reported inert (rc={rc})")
check(text.count("| rrf_k_30") == 2, f"ablation: one rrf_k_30 row per corpus after dedupe (got {text.count('| rrf_k_30')})")
try:
    run_ablation([str(c1), "--runner", str(identical_runner), "--arms", ","])
    arms_rc = 0
except SystemExit as e:
    arms_rc = e.code
check(arms_rc == 2, f"ablation: --arms ',' is an argparse error (rc={arms_rc})")

empty_corpus = tmp / "empty.yaml"
empty_corpus.write_text(f"project_root: {proj}\ncases: []\n")
with patched_search(canned_search):
    rc, out, err = run_runner([str(empty_corpus), "--results-json", str(tmp / "empty.json")])
check(rc == 2 and "no cases to run" in err and "100%" not in out, f"runner: empty cases exits 2 (rc={rc})")
check(not (tmp / "empty.json").exists(), "runner: no envelope written for an empty corpus")
salted_out = tmp / "salted-out.yaml"
salted_out.write_text(f"project_root: {proj}\ncases:\n  - id: only\n    query: q\n    expected_files: [a.rs]\n")
lone_split = runner.split_of("only", "s")
other_split = "heldout" if lone_split == "train" else "train"
with patched_search(canned_search):
    rc, _, err = run_runner([str(salted_out), "--split", other_split, "--salt", "s"])
check(rc == 2 and "no cases to run (--split" in err, f"runner: split selecting zero cases exits 2 (rc={rc})")
rc, text = run_ablation([str(empty_corpus), "--runner", str(HERE / "codesage-bench-runner"), "--arms", "no_definition_boost"])
check(rc == 1 and "| baseline | INVALID (runner rc=2, no METRICS) |" in text
      and "| no_definition_boost | INVALID (runner rc=2, no METRICS) |" in text,
      f"ablation: empty-corpus arms are INVALID via the real runner (rc={rc})")

ndcg = _load("semble-ndcg-runner", "semble_ndcg_under_test")
_orig_ndcg_info = ndcg.codesage_version_info
ndcg.codesage_version_info = lambda _bin, cwd=None: ver.parse_version_banner(BANNER)
try:
    prov = ndcg.codesage_provenance("codesage")
finally:
    ndcg.codesage_version_info = _orig_ndcg_info
check(prov == {"codesage_version": "0.26.1 (release)", "codesage_build_target": "x86_64-unknown-linux",
               "codesage_features": "cpu, cuda"}, f"semble-ndcg: provenance keys from the shared helper (got {prov})")
ndcg.codesage_version_info = lambda _bin, cwd=None: ver.parse_version_banner("")
try:
    prov_unknown = ndcg.codesage_provenance("codesage")
finally:
    ndcg.codesage_version_info = _orig_ndcg_info
check(prov_unknown["codesage_version"] is None and "codesage_device" not in prov_unknown,
      "semble-ndcg: unknown binary keeps the null version and never records device")

leftovers = sorted(p.name for p in (tmp / "nested" / "dir").iterdir())
check(leftovers == ["results.json"], f"runner: only results.json remains in its directory (got {leftovers})")

calls: list[str] = []


def counting_search(_bin, _root, query, _limit):
    calls.append(query)
    return canned, None


unwritable = tmp / "file-not-dir"
unwritable.write_text("x")
with patched_search(counting_search):
    rc, _, err = run_runner([str(run_corpus), "--results-json", str(unwritable / "results.json")])
check(rc != 0 and "not writable" in err, f"runner: unwritable --results-json fails (rc={rc})")
check(calls == [], "runner: no search ran before the path probe failed")

finished = tmp / "finished.json"
finished_bytes = (tmp / "full.json").read_bytes()
finished.write_bytes(finished_bytes)
with patched_search(counting_search):
    rc, _, err = run_runner([str(tmp / "no-such-corpus.yaml"), "--results-json", str(finished)])
check(rc != 0 and "cannot read corpus" in err, f"runner: missing corpus exits nonzero (rc={rc})")
check(finished.read_bytes() == finished_bytes, "runner: existing --results-json untouched when the corpus is missing")
check(not finished.with_name("finished.json.probe").exists(), "runner: probe file removed")
check(calls == [], "runner: no search ran for the missing corpus")

seen: list[dict] = []


def capture_then_stop(*_a, **_k):
    if len(seen) == 2:
        raise KeyboardInterrupt
    seen.append(json.loads((tmp / "partial.json").read_text()))
    return canned, None


with patched_search(capture_then_stop):
    try:
        run_runner([str(run_corpus), "--results-json", str(tmp / "partial.json")])
    except KeyboardInterrupt:
        pass
check(runner.run_codesage_search is not capture_then_stop, "tests: patched_search restores the original")
partial_env = json.loads((tmp / "partial.json").read_text())
check(seen[0]["complete"] is False and seen[0]["records"] == [] and seen[0]["meta"].get("corpus") == "run-corpus.yaml",
      "runner: first envelope is written after validation, with meta and no records")
check(seen[1]["complete"] is False and [r["id"] for r in seen[1]["records"]] == ["r0"],
      "runner: envelope rewritten after each case")
check(partial_env["complete"] is False and [r["id"] for r in partial_env["records"]] == ["r0", "r1"],
      "runner: interrupted run leaves 2 records marked incomplete")


_tmpdir.cleanup()
check(not tmp.exists(), "tests: temporary directory removed")
if failures:
    print(f"FAILED ({len(failures)}):")
    for f in failures:
        print(f)
    sys.exit(1)
print("all compare-runs tests passed")
sys.exit(0)
