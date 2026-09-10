#!/usr/bin/env python3
"""Observe real indexing commits during analysis on a disposable pinned snapshot."""

import argparse
import asyncio
import contextlib
import importlib.util
import json
import math
import os
from pathlib import Path
import shutil
import sqlite3
import time

spec = importlib.util.spec_from_file_location("wal_runner", Path(__file__).with_name("run.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
MAX_SAMPLES = 2000
LOG_LIMIT = 4 * 1024 * 1024
RECOMPUTED_TEXT = "Cached ranking could not be reused; ranking was recomputed for this request's read snapshot."


def environments(inherited):
    effective = {key: value for key, value in inherited.items() if key in runner.PERFORMANCE_ENV}
    daemon = {**effective, "CODESAGE_WATCH": "0", "CODESAGE_DIAGNOSTICS": "1",
              "CODESAGE_OVERVIEW_CACHE": "1"}
    indexer = {key: value for key, value in inherited.items() if not key.startswith("CODESAGE_")}
    indexer.update(effective)
    indexer["CODESAGE_WATCH"] = "0"
    recorded_indexer = {**effective, "CODESAGE_WATCH": "0"}
    return {key: value for key, value in daemon.items() if key != "CODESAGE_WATCH"}, indexer, {
        "daemon": daemon, "indexer": recorded_indexer}


def response_contract(response, require_recomputation=False):
    result = response.get("result", {})
    structured = result.get("structuredContent")
    texts = [row["text"] for row in result.get("content", []) if row.get("type") == "text"]
    objects = []
    for text in texts:
        try:
            value = json.loads(text)
        except (ValueError, TypeError):
            continue
        if isinstance(value, dict):
            objects.append(value)
    if "error" in response or result.get("isError"):
        return {"valid": False, "reason": "request failed", "incomplete_disclosed": any(
            value.get("complete") is False and value.get("next", "missing") is None
            and isinstance(value.get("phase"), str)
            and isinstance(value.get("work_continuing"), bool) for value in objects)}
    if not isinstance(structured, dict) or structured not in objects:
        return {"valid": False, "reason": "missing or inconsistent structured/text result"}
    required = {"project_root": str, "languages": list, "file_count": int, "symbol_count": int,
                "freshness": dict, "feature_summary": list, "feature_count": int,
                "top_risk_files": list, "trust_boundary_clusters": list, "test_conventions": list,
                "entrypoints": list, "suggested_next_calls": list}
    if any(type(structured.get(key)) is not kind for key, kind in required.items()):
        return {"valid": False, "reason": "missing or invalid overview fields"}
    meta = structured.get("_meta", {})
    if structured.get("complete") is False or meta.get("truncated") is True:
        return {"valid": False, "reason": "successful result reports incomplete or truncated output"}
    recomputed = meta.get("ranking_recomputed") is True
    if recomputed != (RECOMPUTED_TEXT in texts):
        return {"valid": False, "reason": "inconsistent recomputation disclosure"}
    if require_recomputation and not recomputed:
        return {"valid": False, "reason": "recomputation disclosure not observed; running phase does not attest snapshot pin timing"}
    return {"valid": True, "ranking_recomputed": recomputed}


async def overview(client, project, timeout):
    started = time.monotonic()
    # This client is dedicated to sequential overview calls; begin registers before sending.
    request_id = client.sequence + 1
    future = None
    try:
        async with asyncio.timeout(timeout):
            request_id, future = await client.begin("tools/call", {
                "name": "project_overview", "arguments": {"project": str(project)}})
            response = await asyncio.shield(future)
        return {"outcome": "tool_error" if "error" in response or response.get("result", {}).get("isError") else "success",
                "wall_s": time.monotonic() - started, "response": response,
                "contract": response_contract(response)}
    except TimeoutError:
        cancellation = "sent"
        try:
            await asyncio.wait_for(client.send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": request_id, "reason": "WAL probe timeout"}}), min(timeout, 1))
        except (TimeoutError, ConnectionError, OSError, ValueError) as error:
            cancellation = type(error).__name__
        return {"outcome": "client_timeout", "wall_s": time.monotonic() - started,
                "cancellation": cancellation,
                "contract": {"valid": False, "reason": "response unavailable"}}
    except (ConnectionError, OSError, ValueError) as error:
        return {"outcome": "transport_error", "error_type": type(error).__name__,
                "wall_s": time.monotonic() - started,
                "contract": {"valid": False, "reason": "response unavailable"}}
    finally:
        registered = client.pending.pop(request_id, None)
        if future is None:
            future = registered
        if future is not None and not future.done():
            future.cancel()
        elif future is not None and not future.cancelled():
            future.exception()


def positive_seconds(value):
    value = float(value)
    if not math.isfinite(value) or not 0 < value <= 3600:
        raise argparse.ArgumentTypeError("seconds must be finite, positive, and at most 3600")
    return value


class Observer:
    def __init__(self, path):
        self.path = path
        self.conn = sqlite3.connect(path.as_uri() + "?mode=rw", uri=True,
                                    timeout=0, check_same_thread=False)
        if self.conn.execute("PRAGMA journal_mode").fetchone()[0] != "wal":
            self.conn.close()
            raise ValueError("index is not in WAL mode")

    def sample(self, mode="PASSIVE"):
        if mode not in {"PASSIVE", "TRUNCATE"}:
            raise ValueError("unsupported checkpoint mode")
        wal = Path(str(self.path) + "-wal")
        before = wal.stat().st_size if wal.exists() else 0
        version = self.conn.execute("PRAGMA data_version").fetchone()[0]
        checkpoint = self.conn.execute(f"PRAGMA wal_checkpoint({mode})").fetchone()
        return {"monotonic_s": time.monotonic(), "data_version": version,
                "wal_bytes_before": before, "checkpoint": list(checkpoint),
                "wal_bytes_after": wal.stat().st_size if wal.exists() else 0}

    def close(self):
        self.conn.close()


async def diagnostics(client):
    result = await runner.stats(client)
    payload = result.get("response", {}).get("structuredContent", {})
    if not result.get("available") or payload.get("enabled") is not True:
        raise ValueError("enabled daemon diagnostics required")
    return payload


def running_analysis(payload):
    return {row["id"] for row in payload["active_executions"]
            if row.get("work_class") == "analysis" and row.get("phase") == "running"}


async def stop(process):
    if process.returncode is None:
        with contextlib.suppress(ProcessLookupError):
            process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 5)
        except TimeoutError:
            with contextlib.suppress(ProcessLookupError):
                process.kill()
            await process.wait()


async def capture(stream, path):
    total = 0
    with path.open("xb") as log:
        while chunk := await stream.read(65536):
            retained = max(0, min(len(chunk), LOG_LIMIT - total))
            log.write(chunk[:retained])
            total += len(chunk)
    return {"path": str(path), "bytes_received": total,
            "bytes_retained": min(total, LOG_LIMIT), "truncated": total > LOG_LIMIT}


async def exercise(binary, project, output, timeout, index_timeout):
    report = {"measurement_complete": False, "samples": [], "overlap_proven": False}
    daemon_env, index_env, report["effective_controls"] = environments(os.environ)
    report["response_bound"] = "Two full JSON-RPC responses; Client limits each received line to 16 MiB. Private output directory."
    observer = await asyncio.to_thread(Observer, project / ".codesage/index.db")
    try:
        async with runner.Daemon(binary, output / "daemon", False,
                                 daemon_env, runner.digest(binary)) as daemon:
            client = await runner.Client.connect(daemon.socket)
            task = asyncio.create_task(overview(client, project, timeout))
            process = pump = None
            try:
                deadline = time.monotonic() + timeout
                while not task.done():
                    active = running_analysis(await diagnostics(daemon.client))
                    if active:
                        break
                    if time.monotonic() >= deadline:
                        raise TimeoutError("analysis did not start")
                    await asyncio.sleep(.02)
                else:
                    report["reason"] = "analysis finished before indexing could start"
                    report["overview"] = await task
                    return report
                previous = await asyncio.to_thread(observer.sample)
                previous_ids = running_analysis(await diagnostics(daemon.client))
                if not previous_ids:
                    report["reason"] = "analysis finished before indexing could start"
                    report["overview"] = await task
                    return report
                report["before_index"] = {**previous, "analysis_ids": sorted(previous_ids)}
                command = [str(binary), "index", "--full", "--no-semantic", "--no-features"]
                report["index"] = {"command": command, "started_unix_s": time.time(),
                                   "started_monotonic_s": time.monotonic()}
                process = await asyncio.create_subprocess_exec(*command, cwd=project, env=index_env,
                            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT)
                pump = asyncio.create_task(capture(process.stdout, output / "index.log"))
                deadline = time.monotonic() + index_timeout
                while process.returncode is None:
                    if time.monotonic() >= deadline:
                        raise TimeoutError("indexing exceeded timeout")
                    if len(report["samples"]) >= MAX_SAMPLES:
                        raise TimeoutError("sample limit reached")
                    before_ids = running_analysis(await diagnostics(daemon.client))
                    sample = await asyncio.to_thread(observer.sample)
                    after_ids = running_analysis(await diagnostics(daemon.client))
                    common = previous_ids & before_ids & after_ids
                    sample["analysis_ids_through_commit_interval"] = sorted(common)
                    sample["committed_since_previous"] = sample["data_version"] != previous["data_version"]
                    sample["index_running_at_observation"] = process.returncode is None
                    if common and sample["committed_since_previous"]:
                        report["overlap_proven"] = True
                    report["samples"].append(sample)
                    previous, previous_ids = sample, after_ids
                    await asyncio.sleep(.1)
                report["index"].update({"exit_code": await process.wait(),
                    "ended_unix_s": time.time(), "ended_monotonic_s": time.monotonic(),
                    "log": await pump})
                report["overview"] = await task
                if "response" in report["overview"]:
                    report["overview"]["contract"] = response_contract(
                        report["overview"]["response"], require_recomputation=report["overlap_proven"])
                deadline = time.monotonic() + timeout
                while True:
                    state = await diagnostics(daemon.client)
                    if state["gauges"]["active_executions"] == 0:
                        report["drained_monotonic_s"] = time.monotonic()
                        break
                    if time.monotonic() >= deadline:
                        raise TimeoutError("tracked work did not drain")
                    await asyncio.sleep(.1)
                report["recovery"] = await asyncio.to_thread(observer.sample, "TRUNCATE")
                report["followup"] = await overview(client, project, timeout)
                report["measurement_complete"] = (
                    report["overlap_proven"] and report["index"]["exit_code"] == 0
                    and report["overview"]["outcome"] == "success"
                    and report["followup"]["outcome"] == "success"
                    and report["overview"]["contract"]["valid"]
                    and report["followup"]["contract"]["valid"]
                    and report["recovery"]["checkpoint"] == [0, 0, 0]
                    and report["recovery"]["wal_bytes_after"] == 0)
                if not report["measurement_complete"]:
                    report["reason"] = "overlap, successful requests/indexing, or checkpoint recovery not established"
            except (TimeoutError, ValueError, OSError, sqlite3.Error) as error:
                report["error"] = {"type": type(error).__name__, "message": str(error)}
            finally:
                if process is not None:
                    await stop(process)
                    report["index"].setdefault("exit_code", process.returncode)
                    report["index"].setdefault("ended_unix_s", time.time())
                    report["index"].setdefault("ended_monotonic_s", time.monotonic())
                    if pump is not None:
                        report["index"]["log"] = await pump
                if not task.done():
                    task.cancel()
                await asyncio.gather(task, return_exceptions=True)
                await client.close()
    finally:
        await asyncio.to_thread(observer.close)
    return report


async def run(args):
    project, binary, output = args.project.resolve(), args.binary.resolve(), args.output.resolve()
    if output.is_relative_to(project) or project.is_relative_to(output):
        raise ValueError("output and input project must be disjoint")
    before = await asyncio.to_thread(runner.verify_snapshot, project)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("binary must be executable")
    for directory, dirs, files in os.walk(project):
        if any((Path(directory) / name).is_symlink() for name in dirs + files):
            raise ValueError("snapshot must not contain symlinks")
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    report = {"measurement_complete": False, "original_before": before,
              "disclosure": "Full structural reindex intentionally mutates the scratch index; scores may change. PASSIVE checkpoints perturb checkpoint scheduling. Original snapshot is never indexed."}
    try:
        scratch = output / "project"
        await asyncio.to_thread(shutil.copytree, project, scratch)
        report["scratch_before"] = await asyncio.to_thread(runner.verify_snapshot, scratch)
        frozen = output / "codesage"
        original_hash = await asyncio.to_thread(runner.digest, binary)
        await asyncio.to_thread(shutil.copy2, binary, frozen)
        if await asyncio.to_thread(runner.digest, frozen) != original_hash:
            raise ValueError("binary changed while copying")
        report["binary_sha256"] = original_hash
        report.update(await exercise(frozen, scratch, output, args.timeout, args.index_timeout))
        report["scratch_database_after_sha256"] = await asyncio.to_thread(runner.digest, scratch / ".codesage/index.db")
    finally:
        try:
            report["original_after"] = await asyncio.to_thread(runner.verify_snapshot, project)
            report["original_unchanged"] = report["original_after"] == before
        except (ValueError, OSError) as error:
            report["original_unchanged"] = False
            report["original_validation_error"] = str(error)
        report["measurement_complete"] &= report["original_unchanged"]
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "project", "output"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--timeout", type=positive_seconds, default=30)
    parser.add_argument("--index-timeout", type=positive_seconds, default=120)
    args = parser.parse_args()
    report = asyncio.run(run(args))
    print(json.dumps({"measurement_complete": report["measurement_complete"],
                      "output": str(args.output.resolve())}))
    return 0 if report["measurement_complete"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
