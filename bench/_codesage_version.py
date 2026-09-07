"""Parse the `codesage --version` banner into scorecard-safe provenance.

Since 0.26 the banner is multi-line:

    codesage 0.26.1 (release)
      target: x86_64-unknown-linux
      features compiled: cpu, cuda
      device configured: gpu

Only the first line may reach a scorecard's `- **CodeSage**:` line, the
quotable one-liner, or the `<!-- METRICS: ... -->` comment, which consumers
grep as a single line. The remaining fields are useful provenance and go into
results meta as `build_target`, `features`, and `device`.

Shared by `codesage-bench-runner`, `agent-task-runner`, and
`semble-ndcg-runner`; each imports it via `sys.path` because their own file
names are not importable module names.
"""
from __future__ import annotations

import subprocess

BANNER_FIELDS = {
    "target": "build_target",
    "features compiled": "features",
    "device configured": "device",
}


def parse_version_banner(out: str) -> dict[str, str]:
    lines = [line.strip() for line in out.splitlines() if line.strip()]
    info: dict[str, str] = {"version": "unknown", "version_token": "unknown"}
    if not lines:
        return info
    first = lines[0]
    # Strip the bin name so headers that prefix with "CodeSage:" don't double
    # up ("CodeSage: codesage 0.4.0"); keep a `(release)`/`(debug)` suffix.
    if first.lower().startswith("codesage "):
        first = first[len("codesage "):].strip()
    info["version"] = first or "unknown"
    # Whitespace-free tokens for the METRICS comment: `0.26.1 (release)`
    # becomes version_token=0.26.1 build=release.
    parts = info["version"].split(None, 1)
    info["version_token"] = parts[0]
    if len(parts) == 2:
        build = parts[1].strip()
        if build.startswith("(") and build.endswith(")"):
            build = build[1:-1].strip()
        if build:
            info["build"] = build.replace(" ", "-")
    for line in lines[1:]:
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        name = BANNER_FIELDS.get(key.strip().lower())
        if name:
            info[name] = value.strip()
    return info


def run_version_banner(codesage_bin: str, timeout: int = 30, cwd=None) -> str:
    """Run `codesage --version`. Pass `cwd=<project root>`: the banner's
    `device configured` line reflects the config found from the cwd."""
    try:
        r = subprocess.run(
            [codesage_bin, "--version"], capture_output=True, text=True,
            timeout=timeout, cwd=cwd,
        )
    except (subprocess.SubprocessError, FileNotFoundError, OSError):
        return ""
    if r.returncode != 0:
        return ""
    return r.stdout or ""


def codesage_version_info(codesage_bin: str, cwd=None) -> dict[str, str]:
    return parse_version_banner(run_version_banner(codesage_bin, cwd=cwd))
