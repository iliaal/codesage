#!/usr/bin/env python3
"""Measure actual isolated CodeSage daemons; Python 3.11+, standard library only."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import hashlib
import json
import math
import os
import platform
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import time
import tomllib
from pathlib import Path

TRANSPORT_CONNECT_TIMEOUT = 10
TRANSPORT_CLEANUP_TIMEOUT = 1

PERFORMANCE_ENV = frozenset({
    "OMP_NUM_THREADS", "OMP_WAIT_POLICY", "ORT_DYLIB_PATH", "HF_HUB_OFFLINE",
    "CODESAGE_COUPLING_RECURRENCE", "CODESAGE_BATCH_SIZE", "CODESAGE_NVIDIA_LIBS",
    "CODESAGE_HF_DOWNLOAD_TIMEOUT_SECS", "CODESAGE_ALLOW_CPU_FALLBACK",
    "CODESAGE_REACH_BUDGET", "CODESAGE_REACH_DEADLINE_MS", "CODESAGE_STALENESS_CHECK",
    "CODESAGE_OVERVIEW_CACHE", "CODESAGE_DIAGNOSTICS", "CODESAGE_BUNDLE_TOKEN_BUDGET",
})


def controlled_environment(entries: list[str]) -> tuple[dict[str, str], dict[str, str]]:
    overrides = {}
    for entry in entries:
        key, separator, value = entry.partition("=")
        if not separator or key not in PERFORMANCE_ENV:
            raise ValueError("--env accepts only documented performance controls")
        overrides[key] = value
    effective = {key: os.environ[key] for key in PERFORMANCE_ENV if key in os.environ}
    effective.update(overrides)
    return overrides, effective


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def backup(source: Path, destination: Path) -> dict:
    destination.mkdir(parents=True, exist_ok=False)
    meta = destination / ".codesage"
    meta.mkdir()
    source_db = source / ".codesage/index.db"
    started = time.monotonic()
    with sqlite3.connect(source_db.as_uri() + "?mode=ro", uri=True) as src:
        with sqlite3.connect(meta / "index.db") as dst:
            def progress(status: int, remaining: int, total: int) -> None:
                if time.monotonic() - started > 120:
                    raise TimeoutError("SQLite backup exceeded 120 seconds")
            src.backup(dst, pages=1024, progress=progress)
            counts = {
                name: dst.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]
                for name in ("files", "symbols", "refs")
            }
            paths = [row[0] for row in dst.execute("SELECT path FROM files")]
    copied = missing = 0
    source_hashes = {}
    for relative in paths:
        rel = Path(relative)
        if rel.is_absolute() or ".." in rel.parts:
            raise ValueError(f"unsafe indexed path: {relative}")
        src_path = (source / rel).resolve()
        if not src_path.is_relative_to(source.resolve()):
            raise ValueError(f"indexed symlink escapes project: {relative}")
        if src_path.is_file():
            dest_path = destination / rel
            dest_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src_path, dest_path)
            source_hashes[relative] = digest(dest_path)
            copied += 1
        else:
            missing += 1
    shutil.copy2(source / ".codesage/config.toml", meta / "config.toml")
    result = {
        "source": str(source), "snapshot": str(destination),
        "database_sha256": digest(meta / "index.db"), "counts": counts,
        "config_sha256": digest(meta / "config.toml"),
        "model_configuration": {key: value for key, value in tomllib.loads((meta / "config.toml").read_text()).get("embedding", {}).items()
                                if key in {"model", "device", "reranker", "batch_size", "pooling"}},
        "source_tree_sha256": hashlib.sha256(json.dumps(source_hashes, sort_keys=True).encode()).hexdigest(),
        "copied_indexed_files": copied, "missing_indexed_files": missing,
        "method": "SQLite online backup; indexed source copied after backup",
        "git_metadata": "omitted; freshness is intentionally not comparable to original project",
        "source_atomicity": "database snapshot is atomic; working files are not an atomic snapshot",
    }
    (destination / "snapshot.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def verify_snapshot(project: Path) -> dict:
    manifest = json.loads((project / "snapshot.json").read_text())
    wal = project / ".codesage/index.db-wal"
    if wal.exists() and wal.stat().st_size:
        raise ValueError(f"snapshot has a nonempty WAL: {project}; create a new online backup")
    for field, relative in (("database_sha256", ".codesage/index.db"), ("config_sha256", ".codesage/config.toml")):
        actual = digest(project / relative)
        if actual != manifest[field]:
            raise ValueError(f"snapshot {field} mismatch: {project}")
    with sqlite3.connect((project / ".codesage/index.db").as_uri() + "?mode=ro", uri=True) as conn:
        paths = [row[0] for row in conn.execute("SELECT path FROM files")]
    hashes = {}
    for relative in paths:
        path = (project / relative).resolve()
        if not path.is_relative_to(project.resolve()):
            raise ValueError("snapshot indexed path escapes project")
        if path.is_file():
            hashes[relative] = digest(path)
    actual_tree = hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest()
    if actual_tree != manifest.get("source_tree_sha256"):
        raise ValueError(f"snapshot source tree hash mismatch or absent: {project}")
    return manifest


def process_cpu(pid: int) -> float:
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def process_memory(pid: int) -> dict:
    try:
        values = {}
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            name, _, value = line.partition(":")
            if name in ("VmHWM", "VmRSS"):
                fields = value.split()
                if len(fields) != 2 or fields[1] != "kB" or not fields[0].isdigit() or name in values:
                    raise ValueError("invalid resident-memory field")
                values[name] = int(fields[0])
        if set(values) != {"VmHWM", "VmRSS"}:
            raise ValueError("missing resident-memory field")
        return {"available": True, "daemon_lifetime_peak_rss_kib": values["VmHWM"],
                "current_rss_kib": values["VmRSS"]}
    except (OSError, ValueError) as error:
        return {"available": False, "error_type": type(error).__name__}


def stable_result(message: dict) -> str:
    result = message.get("result", {})
    structured = result.get("structuredContent")
    if structured is None:
        for item in result.get("content", []):
            if item.get("type") == "text":
                try:
                    structured = json.loads(item["text"])
                    break
                except (ValueError, KeyError):
                    pass
    if structured is None:
        structured = result
    structured = json.loads(json.dumps(structured))
    if isinstance(structured, dict):
        meta = structured.get("_meta")
        if isinstance(meta, dict):
            meta.pop("stale_files", None)
            meta.pop("stale_warning", None)
            if not meta:
                structured.pop("_meta")
        freshness = structured.get("freshness")
        if isinstance(freshness, dict):
            freshness.pop("structural_summary", None)
        for call in structured.get("suggested_next_calls", []):
            if isinstance(call, dict) and call.get("tool") == "codesage index (CLI)":
                call.pop("why", None)
    raw = json.dumps(structured, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(raw.encode()).hexdigest()


def summary(rows: list[dict]) -> dict:
    successful = [row["wall_s"] for row in rows if row["outcome"] == "success"]
    ordered = sorted(successful)
    result = {"requests": len(rows), "successes": len(successful)}
    for outcome in ("tool_error", "protocol_error", "transport_error", "client_timeout"):
        result[outcome] = sum(row["outcome"] == outcome for row in rows)
    for name, percentile, minimum in (("p50_s", .5, 1), ("p95_s", .95, 20), ("p99_s", .99, 100)):
        result[name] = ordered[max(0, math.ceil(len(ordered) * percentile) - 1)] if len(ordered) >= minimum else None
    return result


class Client:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.reader, self.writer = reader, writer
        self.sequence = 0
        self.pending: dict[int, asyncio.Future] = {}
        self.reader_task = asyncio.create_task(self.receive())
        self.close_started = False

    @classmethod
    async def connect(cls, socket: Path) -> Client:
        client = None
        try:
            async with asyncio.timeout(TRANSPORT_CONNECT_TIMEOUT):
                reader, writer = await asyncio.open_unix_connection(socket, limit=16 * 1024 * 1024)
                client = cls(reader, writer)
                await client.request("initialize", {
                    "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": {"name": "daemon-performance", "version": "1"},
                }, TRANSPORT_CONNECT_TIMEOUT)
                await client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        except BaseException:
            if client is not None:
                await client.close()
            raise
        return client

    async def receive(self) -> None:
        try:
            while line := await self.reader.readline():
                value = json.loads(line)
                if not isinstance(value, dict) or value.get("jsonrpc") != "2.0":
                    raise ValueError("invalid JSON-RPC message shape")
                if "method" in value and "id" not in value:
                    if not isinstance(value["method"], str) or "result" in value or "error" in value:
                        raise ValueError("invalid JSON-RPC notification")
                    continue
                if ("id" not in value or not isinstance(value["id"], (str, int, type(None)))
                        or isinstance(value["id"], bool)
                        or "method" in value or ("result" in value) == ("error" in value)):
                    raise ValueError("invalid JSON-RPC response")
                if "result" in value and not isinstance(value["result"], dict):
                    raise ValueError("invalid MCP result")
                if "error" in value:
                    error = value["error"]
                    if (not isinstance(error, dict) or not isinstance(error.get("code"), int)
                            or isinstance(error.get("code"), bool)
                            or not isinstance(error.get("message"), str)):
                        raise ValueError("invalid JSON-RPC error")
                future = self.pending.get(value.get("id"))
                if future is not None and not future.done():
                    future.set_result(value)
        except (OSError, ValueError) as error:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(error)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(ConnectionError("daemon connection closed"))

    async def send(self, value: dict) -> None:
        self.writer.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
        await self.writer.drain()

    def prepare(self) -> tuple[int, asyncio.Future]:
        self.sequence += 1
        request_id = self.sequence
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        return request_id, future

    async def begin(self, method: str, params: dict) -> tuple[int, asyncio.Future]:
        request_id, future = self.prepare()
        try:
            async with asyncio.timeout(TRANSPORT_CONNECT_TIMEOUT):
                await self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        except BaseException:
            self.finish(request_id, future)
            raise
        return request_id, future

    async def request(self, method: str, params: dict, timeout: float) -> dict:
        request_id, future = self.prepare()
        try:
            async with asyncio.timeout(timeout):
                await self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
                return await future
        finally:
            self.finish(request_id, future)

    def finish(self, request_id: int, future: asyncio.Future) -> None:
        self.pending.pop(request_id, None)
        if not future.done():
            future.cancel()
        elif not future.cancelled():
            future.exception()

    async def close(self) -> None:
        if self.close_started:
            return
        self.close_started = True
        self.writer.close()
        try:
            async with asyncio.timeout(TRANSPORT_CLEANUP_TIMEOUT):
                await self.writer.wait_closed()
        except (OSError, TimeoutError):
            self.writer.transport.abort()
        except asyncio.CancelledError:
            self.writer.transport.abort()
            raise
        finally:
            self.reader_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self.reader_task
            for request_id, future in list(self.pending.items()):
                self.finish(request_id, future)


async def call(client: Client, tool: str, arguments: dict, timeout: float, abandon: str | None = None) -> dict:
    started = time.monotonic()
    request_id, future = client.prepare()
    identity = {"tool": tool, "project": arguments.get("project")}
    try:
        async with asyncio.timeout(timeout):
            await client.send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                               "params": {"name": tool, "arguments": arguments}})
            response = await asyncio.shield(future)
        if "error" not in response:
            validate_tool_result(response["result"])
        outcome = "protocol_error" if "error" in response else "tool_error" if response.get("result", {}).get("isError") else "success"
        return {**identity, "wall_s": time.monotonic() - started, "outcome": outcome,
                "stable_sha256": stable_result(response) if outcome == "success" else None}
    except TimeoutError:
        row = {**identity, "outcome": "client_timeout", "abandon_action": "none",
               "server_stopped": "unknown; inspect diagnostics and post-response CPU"}
        try:
            if abandon == "cancel":
                async with asyncio.timeout(TRANSPORT_CLEANUP_TIMEOUT):
                    await client.send({"jsonrpc": "2.0", "method": "notifications/cancelled",
                                       "params": {"requestId": request_id, "reason": "benchmark cancellation"}})
                row["abandon_action"] = "cancel_sent"
            elif abandon == "disconnect":
                await client.close()
                row["abandon_action"] = "transport_closed"
        except (ConnectionError, OSError, ValueError) as error:
            row["abandon_action"] = "cancel_send_failed" if abandon == "cancel" else "transport_close_failed"
            row["abandon_error_type"] = type(error).__name__
        row["wall_s"] = time.monotonic() - started
        return row
    except (ConnectionError, OSError, ValueError) as error:
        return {**identity, "wall_s": time.monotonic() - started, "outcome": "transport_error",
                "error_type": type(error).__name__}
    finally:
        client.finish(request_id, future)

def validate_tool_result(result: dict) -> None:
    content = result.get("content")
    if (not isinstance(content, list)
            or any(not isinstance(item, dict) or not isinstance(item.get("type"), str) for item in content)
            or ("isError" in result and not isinstance(result["isError"], bool))
            or ("structuredContent" in result and not isinstance(result["structuredContent"], dict))):
        raise ValueError("invalid MCP tool result")


async def stats(client: Client) -> dict:
    response = await client.request("tools/call", {"name": "daemon_stats", "arguments": {"recent": 256}}, 5)
    if "error" not in response:
        validate_tool_result(response["result"])
    if "error" in response or response.get("result", {}).get("isError"):
        return {"available": False, "reason": "daemon_stats unsupported or unsuccessful"}
    return {"available": True, "response": response["result"]}


class Daemon:
    def __init__(self, binary: Path, runtime: Path, watcher: bool, extra_env: dict[str, str], expected_sha256: str):
        self.binary, self.runtime, self.watcher, self.extra_env = binary, runtime, watcher, extra_env
        self.expected_sha256 = expected_sha256

    async def __aenter__(self) -> Daemon:
        if digest(self.binary) != self.expected_sha256:
            raise ValueError("binary copy differs from frozen manifest hash")
        self.runtime.mkdir(parents=True, exist_ok=False)
        self.socket_runtime = tempfile.TemporaryDirectory(prefix="csp-")
        inherited = {key: value for key, value in os.environ.items()
                     if not key.startswith("CODESAGE_") or key in PERFORMANCE_ENV}
        env = dict(inherited, CODESAGE_DAEMON_RUNTIME_DIR=self.socket_runtime.name,
                   CODESAGE_WATCH="1" if self.watcher else "0", **self.extra_env)
        self.log = (self.runtime / "foreground.log").open("wb")
        self.process = await asyncio.create_subprocess_exec(str(self.binary), "daemon", env=env,
                                                            stdout=self.log, stderr=self.log)
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                sockets = list(Path(self.socket_runtime.name).glob("*.sock"))
                if sockets:
                    self.client = await Client.connect(sockets[0])
                    self.socket = sockets[0]
                    if digest(Path(f"/proc/{self.process.pid}/exe")) != self.expected_sha256:
                        raise ValueError("launched executable differs from frozen manifest hash")
                    return self
                if self.process.returncode is not None:
                    raise RuntimeError(f"daemon exited {self.process.returncode}; inspect {self.runtime}")
                await asyncio.sleep(.02)
            raise TimeoutError("daemon socket did not appear")
        except BaseException:
            await self.__aexit__(None, None, None)
            raise

    async def __aexit__(self, *_args) -> None:
        if hasattr(self, "client"):
            await self.client.close()
        if self.process.returncode is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                await asyncio.wait_for(self.process.wait(), 15)
            except TimeoutError:
                self.process.kill()
                await self.process.wait()
        self.log.close()
        self.socket_runtime.cleanup()


async def measure(daemon: Daemon, name: str, requests: list[tuple[str, dict]], timeout: float,
                  concurrent: bool = False, abandon: str | None = None, observe: float = 0,
                  max_inflight: int | None = None) -> dict:
    if max_inflight is not None and (not concurrent or max_inflight < 1):
        raise ValueError("max_inflight requires concurrent requests and a positive limit")
    report = {"scenario": name, "summary": summary([]), "requests": [], "measurement_complete": False}
    if max_inflight is not None:
        report["client_scheduling"] = {"max_inflight": max_inflight,
            "method": "client semaphore; scheduling wait excluded from request wall_s and MCP timeout, included in wall_to_responses_s"}
    clients = []
    phase = "setup"
    try:
        report["stats_before"] = await stats(daemon.client)
        report["memory_before"] = process_memory(daemon.process.pid)
        cpu_before = process_cpu(daemon.process.pid)
        started = time.monotonic()
        for _ in requests:
            clients.append(await Client.connect(daemon.socket))
        if concurrent:
            semaphore = asyncio.Semaphore(max_inflight) if max_inflight is not None else None
            async def scheduled_call(client, tool, args):
                if semaphore is None:
                    return await call(client, tool, args, timeout, abandon)
                queued = time.monotonic()
                async with semaphore:
                    wait = time.monotonic() - queued
                    row = await call(client, tool, args, timeout, abandon)
                    row["client_scheduling_wait_s"] = wait
                    return row
            async with asyncio.TaskGroup() as group:
                tasks = [group.create_task(scheduled_call(client, tool, args))
                         for client, (tool, args) in zip(clients, requests)]
            rows = [task.result() for task in tasks]
        else:
            rows = [await call(client, tool, args, timeout, abandon)
                    for client, (tool, args) in zip(clients, requests)]
        response_wall = time.monotonic() - started
        response_cpu = process_cpu(daemon.process.pid) - cpu_before
        cpu_samples = []
        report.update({"summary": summary(rows), "requests": rows,
                       "by_project": {project: summary([row for row in rows if row.get("project") == project])
                                      for project in sorted({row.get("project", "") for row in rows})},
                       "wall_to_responses_s": response_wall, "process_cpu_to_responses_s": response_cpu,
                       "post_response_observation_s": observe, "post_response_cpu_samples": cpu_samples})
        phase = "observation"
        observation_start = time.monotonic()
        while time.monotonic() - observation_start < observe:
            await asyncio.sleep(min(.1, max(0, observe - (time.monotonic() - observation_start))))
            cpu_samples.append({"elapsed_s": time.monotonic() - observation_start,
                                "cpu_s": process_cpu(daemon.process.pid) - cpu_before - response_cpu})
        total_cpu = process_cpu(daemon.process.pid) - cpu_before
        report["process_cpu_after_responses_s"] = total_cpu - response_cpu
        report["stats_after"] = await stats(daemon.client)
        report["measurement_complete"] = True
    except (ConnectionError, OSError, TimeoutError, ValueError) as error:
        report.update({"outcome": "setup_or_observation_error", "error_type": type(error).__name__,
                       "failed_phase": phase})
    finally:
        report["memory_after"] = process_memory(daemon.process.pid)
        await asyncio.gather(*(client.close() for client in clients))
    return report


async def measure_review(daemon: Daemon, project: Path, symbol: str, timeout: float,
                         max_inflight: int | None = None) -> dict:
    requests = [("project_overview", {"project": str(project)})] * 23
    requests += [("find_references", {"project": str(project), "name": symbol})] * 38
    requests += [("session_start", {"project": str(project)})]
    name = "review_bounded" if max_inflight is not None else "review"
    report = await measure(daemon, name, requests, timeout, True, max_inflight=max_inflight)
    scheduling = "bounded client scheduling" if max_inflight is not None else "concurrent burst"
    report["substitution"] = f"62 recoverable calls of observed 104; repeated operator-supplied symbol; {scheduling} substitutes unavailable original timing"
    return report


async def measure_cold_warm(daemon: Daemon, overview: tuple[str, dict], timeout: float) -> list[dict]:
    cold = await measure(daemon, "cold", [overview], timeout)
    if cold["summary"]["successes"] != 1:
        return [cold, {"scenario": "warm", "skipped": "cold request did not complete successfully"}]
    return [cold, await measure(daemon, "warm", [overview] * 20, timeout)]


async def measure_models(daemon: Daemon, search: tuple[str, dict], timeout: float) -> list[dict]:
    initial = await measure(daemon, "model_initialization", [search], timeout)
    if initial["summary"]["successes"] != 1:
        return [initial, {"scenario": "model_warm", "skipped": "initialization did not complete successfully"}]
    return [initial, await measure(daemon, "model_warm", [search] * 20, timeout)]


async def measure_model_concurrent(daemon: Daemon, search: tuple[str, dict], timeout: float,
                                   concurrency: int, second_project: Path | None, symbol: str,
                                   observe: float) -> list[dict]:
    initial = await measure(daemon, "model_concurrent_initialization", [search], timeout)
    if not initial["measurement_complete"] or initial["summary"]["successes"] != 1:
        return [initial, {"scenario": "model_concurrent", "skipped": "initialization did not complete successfully"}]
    requests = [search] * concurrency
    if second_project is not None:
        requests.append(("find_symbol", {"project": str(second_project), "name": symbol}))
    return [initial, await measure(daemon, "model_concurrent", requests, timeout, True, observe=observe)]


def watcher_status(daemon: Daemon, project: Path) -> dict | None:
    try:
        status = json.loads((project / ".codesage/watch.status").read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(status, dict) or status.get("pid") != daemon.process.pid or daemon.process.returncode is not None:
        return None
    return status


def watcher_quiescent(status: dict | None) -> bool:
    return (status is not None and status.get("startup_reconciled") is True
            and status.get("reconciliation_pending") is False
            and status.get("reconciliation_parked") is False
            and isinstance(status.get("stale_parked"), int)
            and not isinstance(status["stale_parked"], bool) and status["stale_parked"] == 0)


async def wait_watcher_quiescent(daemon: Daemon, project: Path, timeout: float) -> dict:
    started = time.monotonic()
    deadline = started + timeout
    quiet_since = None
    samples = 0
    status = watcher_status(daemon, project)
    while True:
        now = time.monotonic()
        samples += 1
        if watcher_quiescent(status):
            quiet_since = now if quiet_since is None else quiet_since
            if now - quiet_since >= .5:
                return {"quiescent": True, "wall_s": now - started, "samples": samples, "status": status}
        else:
            quiet_since = None
        if now >= deadline:
            return {"quiescent": False, "wall_s": now - started, "samples": samples, "status": status}
        await asyncio.sleep(min(.05, deadline - now))
        status = watcher_status(daemon, project)


def indexed_watcher_probe(project: Path, relative: str, expected_hash: str, model: str) -> dict:
    try:
        with sqlite3.connect((project / ".codesage/index.db").as_uri() + "?mode=ro", uri=True, timeout=.1) as db:
            structural = db.execute("SELECT content_hash FROM files WHERE path = ?", (relative,)).fetchone()
            semantic = db.execute(
                "SELECT sf.content_hash FROM semantic_files sf JOIN semantic_models sm "
                "ON sm.chunk_table = sf.chunk_table WHERE sf.path = ? AND sm.model = ?",
                (relative, model)).fetchall()
        return {"structural": structural == (expected_hash,),
                "semantic": any(row == (expected_hash,) for row in semantic)}
    except sqlite3.Error as error:
        return {"structural": False, "semantic": False, "error_type": type(error).__name__}


async def measure_watcher(daemon: Daemon, project: Path, timeout: float, observe: float,
                          query: str = "request cancellation", watcher_timeout: float = 120) -> list[dict]:
    setup = await measure(daemon, "watcher_start", [("search", {"project": str(project), "query": query, "limit": 5})], timeout)
    rows = [setup]
    config = tomllib.loads((project / ".codesage/config.toml").read_text())
    reason = None
    if setup["summary"]["successes"] != 1:
        reason = "setup request did not complete successfully"
    elif config.get("index", {}).get("watch") is False or (project / ".codesage/watch.disabled").exists():
        reason = "project watcher disabled"
    else:
        deadline = time.monotonic() + min(timeout, 10)
        while watcher_status(daemon, project) is None and time.monotonic() < deadline:
            await asyncio.sleep(.05)
        if watcher_status(daemon, project) is None:
            reason = "no active watcher status owned by measured daemon"
    if reason is not None:
        return rows + [{"scenario": name, "unavailable": reason} for name in ("watcher_idle", "watcher_active")]
    setup["verified_watcher_status"] = watcher_status(daemon, project)
    startup = await wait_watcher_quiescent(daemon, project, watcher_timeout)
    setup["startup_quiescence"] = startup
    if not startup["quiescent"]:
        return rows + [{"scenario": name, "unavailable": "watcher startup/background quiescence not evidenced within timeout"}
                       for name in ("watcher_idle", "watcher_active")]
    before = process_cpu(daemon.process.pid)
    started = time.monotonic()
    deadline = started + observe
    quiet = True
    samples = 0
    while time.monotonic() < deadline:
        await asyncio.sleep(min(.05, max(0, deadline - time.monotonic())))
        quiet = watcher_quiescent(watcher_status(daemon, project)) and quiet
        samples += 1
    rows.append({"scenario": "watcher_idle" if quiet else "watcher_background", "wall_s": time.monotonic() - started,
                 "process_cpu_s": process_cpu(daemon.process.pid) - before,
                 "watcher_active_after": watcher_status(daemon, project) is not None,
                 "measurement_complete": quiet, "quiescence_samples": samples,
                 "completion": "startup reconciled; pending-work status sampled every 50 ms"})
    if not quiet:
        rows.append({"scenario": "watcher_idle", "unavailable": "watcher became unavailable or reported background work during observation"})
    if watcher_status(daemon, project) is None:
        return rows + [{"scenario": "watcher_active", "unavailable": "watcher stopped during idle observation"}]
    touched = project / "daemon_performance_watcher_probe.rs"
    if touched.exists():
        raise FileExistsError(touched)
    try:
        before = process_cpu(daemon.process.pid)
        started = time.monotonic()
        probe_value = int.from_bytes(os.urandom(4))
        touched.write_text(f"pub fn daemon_performance_watcher_probe() -> usize {{ {probe_value} }}\n")
        expected_hash = digest(touched)
        deadline = started + watcher_timeout
        quiet_since = None
        complete = False
        while True:
            model = config.get("embedding", {}).get("model", "sentence-transformers/all-MiniLM-L6-v2")
            indexed = indexed_watcher_probe(project, touched.name, expected_hash, model)
            now = time.monotonic()
            if indexed["structural"] and indexed["semantic"] and watcher_quiescent(watcher_status(daemon, project)):
                quiet_since = now if quiet_since is None else quiet_since
                complete = now - quiet_since >= .5 and now - started >= observe
            else:
                quiet_since = None
            if complete or now >= deadline:
                break
            await asyncio.sleep(min(.05, deadline - now))
        active = watcher_status(daemon, project) is not None
        rows.append({"scenario": "watcher_active", "wall_s": time.monotonic() - started,
                     "process_cpu_s": process_cpu(daemon.process.pid) - before,
                     "watcher_active_after": active, "measurement_complete": complete,
                     "indexed_probe": indexed, "probe_sha256": expected_hash,
                     "completion": "matching structural and configured-model semantic hashes; watcher quiescent for 500 ms; minimum observation elapsed"
                                   if complete else "timeout before indexed probe, quiescence, and minimum observation were evidenced"})
    finally:
        touched.unlink(missing_ok=True)
    return rows


def record_input_validation(result: dict, projects: list[Path]) -> None:
    result["input_after_run"] = []
    result["invalid_inputs"] = []
    for project in projects:
        try:
            manifest = verify_snapshot(project)
            result["input_after_run"].append({"project": str(project), "valid": True,
                "database_sha256": manifest["database_sha256"], "config_sha256": manifest["config_sha256"],
                "source_tree_sha256": manifest["source_tree_sha256"], "nonempty_wal": False})
        except (OSError, ValueError, sqlite3.Error, KeyError) as error:
            failure = {"project": str(project), "valid": False, "error_type": type(error).__name__,
                       "reason": "post-run database/config/source/WAL verification failed"}
            result["input_after_run"].append(failure)
            result["invalid_inputs"].append(failure)
    result["input_validation"] = "invalid_inputs" if result["invalid_inputs"] else "valid"


async def run(args: argparse.Namespace) -> dict:
    binary = args.binary.resolve()
    projects = [project.resolve() for project in args.project]
    output = args.output.resolve()
    if any(output.is_relative_to(project) or project.is_relative_to(output) for project in projects):
        raise ValueError("benchmark output and input projects must be disjoint")
    output.mkdir(parents=True, exist_ok=False)
    supplied_binary = binary
    binary = output / "codesage"
    shutil.copy2(supplied_binary, binary)
    result = {
        "schema_version": 2, "binary": str(binary), "supplied_binary": str(supplied_binary), "binary_sha256": digest(binary),
        "source_commit": args.source_commit, "build_features": args.build_features,
        "source_provenance": "operator-supplied commit/features; executable is copied, hashed, and verified against /proc/PID/exe on each launch",
        "binary_version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
        "host": {"platform": platform.platform(), "cpu_count": os.cpu_count(), "clock_ticks": os.sysconf("SC_CLK_TCK")},
        "cpu_method": "/proc/PID/stat utime+stime for isolated foreground daemon, includes native threads; excludes harness/children",
        "memory_method": "/proc/PID/status VmHWM is daemon-lifetime peak RSS, not interval peak; VmRSS is current RSS; KiB; excludes harness/children",
        "wall_method": "time.monotonic; response interval includes client handshakes; individual request wall starts at send",
        "parity_exclusions": ["_meta.stale_files", "_meta.stale_warning", "freshness.structural_summary", "suggested_next_calls[tool='codesage index (CLI)'].why"],
        "projects": [verify_snapshot(p) for p in projects],
        "workload": {"timeout_s": args.timeout, "abandon_after_s": args.abandon_after,
                     "watcher_timeout_s": args.watcher_timeout,
                     "observe_s": args.observe, "concurrency": args.concurrency,
                     "symbol_sha256": hashlib.sha256(args.symbol.encode()).hexdigest(),
                     "query_sha256": hashlib.sha256(args.query.encode()).hexdigest()},
        "scenarios": [], "rounds": args.rounds, "input_validation": "pending",
        "optional_scenarios": {"model": "enabled" if args.models or "model_concurrent" in args.scenarios.split(",") else "not requested; no inference measurement",
                               "watcher": "enabled" if args.watcher else "not requested; no watcher measurement"},
    }
    env, effective = controlled_environment(args.env)
    result["effective_performance_environment"] = effective
    result["environment_policy"] = "allowlisted controls recorded; other inherited CODESAGE controls removed; credentials are not recorded"
    overview = ("project_overview", {"project": str(projects[0])})
    for round_number in range(args.rounds):
        for scenario in args.scenarios.split(","):
            async with Daemon(binary, output / f"runtime-{round_number}-{scenario}", False, env, result["binary_sha256"]) as daemon:
                if scenario == "cold_warm":
                    result["scenarios"].extend(await measure_cold_warm(daemon, overview, args.timeout))
                elif scenario == "concurrent":
                    result["scenarios"].append(await measure(daemon, scenario, [overview] * args.concurrency, args.timeout, True))
                elif scenario == "mixed":
                    if len(projects) < 2:
                        result["scenarios"].append({"scenario": scenario, "skipped": "requires two independently copied project roots"})
                    else:
                        requests = [overview] * args.concurrency + [("project_overview", {"project": str(projects[1])})]
                        result["scenarios"].append(await measure(daemon, scenario, requests, args.timeout, True))
                elif scenario in ("cancel", "disconnect", "local_timeout"):
                    result["scenarios"].append(await measure(daemon, scenario, [overview] * args.concurrency,
                        args.abandon_after, True, scenario, args.observe))
                elif scenario in ("review", "review_bounded"):
                    result["scenarios"].append(await measure_review(daemon, projects[0], args.symbol, args.timeout,
                        args.concurrency if scenario == "review_bounded" else None))
                elif scenario == "model_concurrent":
                    search = ("search", {"project": str(projects[0]), "query": args.query, "limit": 5})
                    result["scenarios"].extend(await measure_model_concurrent(daemon, search, args.timeout,
                        args.concurrency, projects[1] if len(projects) > 1 else None, args.symbol, args.observe))
                else:
                    raise ValueError(f"unknown scenario {scenario}")
                for row in result["scenarios"]:
                    row.setdefault("round", round_number)
            (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    if args.models:
        async with Daemon(binary, output / "runtime-model", False, env, result["binary_sha256"]) as daemon:
            search = ("search", {"project": str(projects[0]), "query": args.query, "limit": 5})
            result["scenarios"].extend(await measure_models(daemon, search, args.timeout))
    if args.watcher:
        watcher_project = output / "watcher-project"
        shutil.copytree(projects[0], watcher_project)
        async with Daemon(binary, output / "runtime-watcher", True, env, result["binary_sha256"]) as daemon:
            result["scenarios"].extend(await measure_watcher(daemon, watcher_project, args.timeout, args.observe,
                                                            args.query, args.watcher_timeout))
    record_input_validation(result, projects)
    (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    snap = sub.add_parser("snapshot")
    snap.add_argument("--project", type=Path, required=True)
    snap.add_argument("--output", type=Path, required=True)
    bench = sub.add_parser("run")
    bench.add_argument("--binary", type=Path, required=True)
    bench.add_argument("--project", type=Path, action="append", required=True)
    bench.add_argument("--output", type=Path, required=True)
    bench.add_argument("--source-commit", required=True)
    bench.add_argument("--build-features", default="cuda")
    bench.add_argument("--rounds", type=int, default=1)
    bench.add_argument("--concurrency", type=int, default=16,
                       help="burst size for concurrent workloads; maximum simultaneous client calls for review_bounded")
    bench.add_argument("--timeout", type=float, default=30)
    bench.add_argument("--watcher-timeout", type=float, default=120,
                       help="seconds allowed separately for watcher startup reconciliation and active probe completion")
    bench.add_argument("--abandon-after", type=float, default=.05)
    bench.add_argument("--observe", type=float, default=5)
    bench.add_argument("--scenarios", default="cold_warm,concurrent,mixed,cancel,disconnect,local_timeout,review",
                       help="comma-separated scenarios; review is a 62-call burst, review_bounded limits client concurrency; model_concurrent initializes then bursts warmed searches")
    bench.add_argument("--symbol", default="assess_risk_batch")
    bench.add_argument("--query", default="project risk analysis")
    bench.add_argument("--models", action="store_true")
    bench.add_argument("--watcher", action="store_true")
    bench.add_argument("--env", action="append", default=[])
    args = parser.parse_args()
    if args.command == "snapshot":
        print(json.dumps(backup(args.project.resolve(), args.output.resolve()), indent=2))
    else:
        timings = (args.timeout, args.watcher_timeout, args.abandon_after, args.observe)
        if args.rounds < 1 or not 1 <= args.concurrency <= 256 or any(not math.isfinite(value) or value <= 0 for value in timings):
            parser.error("rounds/concurrency/timing values out of range")
        result = asyncio.run(run(args))
        print(json.dumps({"output": str(args.output), "scenarios": len(result["scenarios"])}))


if __name__ == "__main__":
    main()
