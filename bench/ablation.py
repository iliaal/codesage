#!/usr/bin/env python3
"""Ablation sweep over CodeSage ranker knobs — no daemon, existing infra only.

Answers "is the bespoke ranker earning its keep?" by toggling CodeSage's own
ranking stages and re-running the EXISTING bench (codesage-bench-runner) against
each corpus. Each stage is isolated, so you see its individual contribution to
recall — a cleaner experiment than swapping engines (which confounds engine +
ranker + ANN at once).

How it works: the CLI `codesage search` runs in-process and reads its tuning
knobs from env vars on every invocation (the constants are cached per-process
via OnceLock, and each CLI call is a fresh process). So we sweep arms purely by
varying the subprocess environment — nothing persistent, nothing to install.

Arms and the env var each flips:

  baseline            (shipped behavior, no overrides)
  rrf_k_30            CODESAGE_RRF_K=30          amplify rank position   [needs patch]
  rrf_k_120           CODESAGE_RRF_K=120         flatten rank influence  [needs patch]
  bm25_weight_1.0     CODESAGE_BM25_WEIGHT=1.0   symmetric fusion        [needs patch]
  bm25_weight_3.0     CODESAGE_BM25_WEIGHT=3.0   heavier lexical         [needs patch]
  no_definition_boost CODESAGE_DEFINITION_BOOST=0                        [already wired]
  no_path_penalty     CODESAGE_PATH_PENALTY=0                            [already wired]
  qualified_name_boost CODESAGE_QUALIFIED_NAME_BOOST=1                   [already wired]

The four [needs patch] arms require ablation-fusion-weight-patch.md applied (exposes RRF_K and
BM25_WEIGHT as env overrides). Until then the harness still runs them, detects
they produced metrics identical to baseline, and WARNS that the knob is unwired
— so you never read a null result as "the knob doesn't matter."

Usage:
  ./ablation.py corpus1.yaml [corpus2.yaml ...] \
      [--codesage-bin codesage] [--limit 10] \
      [--runner ~/ai/codesage/bench/codesage-bench-runner] \
      [--arms baseline,bm25_weight_1.0,...] \
      [--out ablation-scorecard.md]
"""
from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

# DOTALL: the runner's METRICS payload can contain embedded newlines when
# `codesage --version` emits a multi-line banner (0.13.0 does). Splitting the
# captured group on any whitespace still recovers the key=value metric tokens.
METRICS_RE = re.compile(r"<!--\s*METRICS:\s*(.*?)\s*-->", re.DOTALL)

# (arm name, env overrides, needs_patch)
ARMS: dict[str, tuple[dict[str, str], bool]] = {
    "baseline": ({}, False),
    "rrf_k_30": ({"CODESAGE_RRF_K": "30"}, True),
    "rrf_k_45": ({"CODESAGE_RRF_K": "45"}, True),
    "rrf_k_120": ({"CODESAGE_RRF_K": "120"}, True),
    "bm25_weight_1.0": ({"CODESAGE_BM25_WEIGHT": "1.0"}, True),
    "bm25_weight_1.5": ({"CODESAGE_BM25_WEIGHT": "1.5"}, True),
    "bm25_weight_3.0": ({"CODESAGE_BM25_WEIGHT": "3.0"}, True),
    "no_definition_boost": ({"CODESAGE_DEFINITION_BOOST": "0"}, False),
    "no_path_penalty": ({"CODESAGE_PATH_PENALTY": "0"}, False),
    "qualified_name_boost": ({"CODESAGE_QUALIFIED_NAME_BOOST": "1"}, False),
}

# Metrics we pull from the runner's machine-readable METRICS comment.
METRIC_KEYS = ["miss_rate", "median_first", "r5", "r10", "mean_tokens_to_hit"]


def parse_metrics(stdout: str) -> dict[str, str]:
    m = METRICS_RE.search(stdout)
    if not m:
        return {}
    out: dict[str, str] = {}
    for tok in m.group(1).split():
        if "=" in tok:
            k, v = tok.split("=", 1)
            out[k] = v
    return out


def run_arm(runner: Path, corpus: Path, bin_: str, limit: int, env_over: dict[str, str]) -> dict[str, str]:
    env = {**os.environ, **env_over}
    cmd = [sys.executable, str(runner), str(corpus), "--codesage-bin", bin_, "--limit", str(limit)]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=3600)
    if r.returncode != 0:
        print(f"  [warn] runner rc={r.returncode}: {r.stderr.strip()[:200]}", file=sys.stderr)
    metrics = parse_metrics(r.stdout)
    if not metrics:
        print(f"  [warn] no METRICS line parsed; stderr: {r.stderr.strip()[:200]}", file=sys.stderr)
    return metrics


def metrics_signature(m: dict[str, str]) -> tuple:
    return tuple(m.get(k, "") for k in METRIC_KEYS)


def fmt_cell(m: dict[str, str], key: str) -> str:
    v = m.get(key, "—")
    return v


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("corpora", nargs="+", type=Path)
    ap.add_argument("--codesage-bin", default="codesage")
    ap.add_argument("--limit", type=int, default=10)
    ap.add_argument(
        "--runner",
        type=Path,
        default=Path.home() / "ai/codesage/bench/codesage-bench-runner",
    )
    ap.add_argument("--arms", default="", help="comma-separated subset of arm names")
    ap.add_argument("--out", type=Path, default=None)
    args = ap.parse_args()

    if not args.runner.exists():
        sys.exit(f"runner not found: {args.runner}")

    selected = list(ARMS)
    if args.arms:
        selected = [a.strip() for a in args.arms.split(",") if a.strip()]
        bad = [a for a in selected if a not in ARMS]
        if bad:
            sys.exit(f"unknown arms: {bad}; valid: {list(ARMS)}")
    if "baseline" not in selected:
        selected = ["baseline", *selected]  # always need baseline for deltas

    lines: list[str] = ["# CodeSage ranker ablation", ""]
    lines.append(f"- Runner: `{args.runner}`")
    lines.append(f"- codesage: `{args.codesage_bin}`, top-{args.limit}")
    lines.append(f"- Arms: {', '.join(selected)}")
    lines.append("")

    lines.append(
        "- Marker: `=base` = metrics identical to baseline on that corpus (the "
        "knob had no effect there — e.g. the hybrid gate never fired for these "
        "queries). Identical aggregate metrics do NOT prove the knob is unwired; "
        "see the inert-everywhere note for how to disambiguate."
    )
    lines.append("")

    # Pass 1: run every arm on every corpus, collect results. Track which arms
    # moved off baseline on AT LEAST ONE corpus — that proves the knob is wired.
    all_results: list[tuple[str, dict[str, dict[str, str]]]] = []
    ever_differed: set[str] = set()
    for corpus in args.corpora:
        print(f"[corpus] {corpus.name}", file=sys.stderr)
        results: dict[str, dict[str, str]] = {}
        for arm in selected:
            env_over, _ = ARMS[arm]
            print(f"  [arm] {arm} env={env_over or '{}'}", file=sys.stderr)
            results[arm] = run_arm(args.runner, corpus, args.codesage_bin, args.limit, env_over)
        base_sig = metrics_signature(results.get("baseline", {}))
        for arm in selected:
            if arm == "baseline":
                continue
            if base_sig and metrics_signature(results[arm]) != base_sig:
                ever_differed.add(arm)
        all_results.append((corpus.name, results))

    # Arms that never moved off baseline on ANY corpus. This is NOT proof the
    # knob is unwired — it may legitimately have no effect on these corpora (a
    # larger RRF_K often doesn't reorder anything). It only flags "no measurable
    # effect anywhere tested", which is worth surfacing but must be disambiguated
    # from a missing patch by a raw-output probe (see the rendered note).
    inert_everywhere = {
        a for a in selected
        if a != "baseline" and ARMS[a][1] and a not in ever_differed
    }

    # Pass 2: render.
    for corpus_name, results in all_results:
        base_sig = metrics_signature(results.get("baseline", {}))
        lines.append(f"## {corpus_name}")
        lines.append("")
        lines.append("| arm | miss | median 1st | r@5 | r@10 | tokens→hit | Δr@10 vs base |")
        lines.append("|---|---:|---:|---:|---:|---:|---:|")
        base_r10 = results.get("baseline", {}).get("r10")
        for arm in selected:
            m = results[arm]
            d = ""
            if base_r10 and m.get("r10"):
                try:
                    d = f"{float(m['r10']) - float(base_r10):+.4f}"
                except ValueError:
                    d = ""
            if arm != "baseline" and base_sig and metrics_signature(m) == base_sig:
                note = "  =base"
            else:
                note = ""
            lines.append(
                f"| {arm}{note} | {fmt_cell(m,'miss_rate')} | {fmt_cell(m,'median_first')} "
                f"| {fmt_cell(m,'r5')} | {fmt_cell(m,'r10')} | {fmt_cell(m,'mean_tokens_to_hit')} | {d} |"
            )
        lines.append("")

    if inert_everywhere:
        lines.append("## No measurable effect on any corpus")
        lines.append("")
        lines.append(
            "These arms produced metrics identical to baseline on every corpus "
            "tested. That can mean the knob genuinely doesn't move these corpora "
            "(plausible — a larger RRF_K rarely reorders), OR that the env override "
            "isn't wired. Aggregate metrics can't tell them apart. Disambiguate "
            "with a raw-output probe on a gate-firing query, e.g.:"
        )
        lines.append("")
        lines.append("```")
        lines.append("cd <project_root>")
        lines.append("B=<bin>; Q='some_identifier::method rare_token'")
        lines.append('diff <("$B" search --json -l8 "$Q") \\')
        lines.append('     <(CODESAGE_RRF_K=1 "$B" search --json -l8 "$Q")')
        lines.append("# differs => wired (this arm just had no effect); identical => suspect patch/daemon")
        lines.append("```")
        lines.append("")
        for a in sorted(inert_everywhere):
            lines.append(f"- `{a}` → {ARMS[a][0]}")
        lines.append("")

    text = "\n".join(lines)
    if args.out:
        args.out.write_text(text)
        print(f"[done] wrote {args.out}", file=sys.stderr)
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
