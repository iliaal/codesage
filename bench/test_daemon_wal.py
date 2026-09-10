"""Real SQLite checkpoint and subprocess boundary tests, not performance evidence."""

import argparse
import asyncio
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

spec = importlib.util.spec_from_file_location("wal_overlap", Path(__file__).parent / "daemon-performance/wal_overlap.py")
wal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wal)


class CheckpointTests(unittest.TestCase):
    def test_held_reader_retains_frames_then_truncate_recovers(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "index.db"
            writer = sqlite3.connect(path)
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("CREATE TABLE evidence(value)")
            writer.execute("INSERT INTO evidence VALUES (1)")
            writer.commit()
            reader = sqlite3.connect(path)
            observer = wal.Observer(path)
            try:
                reader.execute("BEGIN")
                self.assertEqual(reader.execute("SELECT value FROM evidence").fetchall(), [(1,)])
                before = observer.sample()
                writer.execute("INSERT INTO evidence VALUES (2)")
                writer.commit()
                held = observer.sample()
                self.assertNotEqual(held["data_version"], before["data_version"])
                self.assertGreater(held["wal_bytes_before"], 0)
                self.assertGreater(held["checkpoint"][1], held["checkpoint"][2])
                self.assertEqual(observer.sample("TRUNCATE")["checkpoint"][0], 1)
                reader.rollback()
                recovered = observer.sample("TRUNCATE")
                self.assertEqual(recovered["checkpoint"], [0, 0, 0])
                self.assertEqual(recovered["wal_bytes_after"], 0)
                self.assertEqual(reader.execute("SELECT value FROM evidence").fetchall(), [(1,), (2,)])
            finally:
                observer.close()
                reader.close()
                writer.close()

    def test_missing_database_is_not_created(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "missing.db"
            with self.assertRaises(sqlite3.OperationalError):
                wal.Observer(path)
            self.assertFalse(path.exists())

    def test_non_wal_database_is_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "index.db"
            with sqlite3.connect(path) as db:
                db.execute("CREATE TABLE evidence(value)")
            with self.assertRaisesRegex(ValueError, "not in WAL mode"):
                wal.Observer(path)


class InputTests(unittest.TestCase):
    def test_effective_controls_match_separate_environments_without_secrets(self):
        inherited = {"PATH": "/bin", "SECRET_TOKEN": "secret", "CODESAGE_OTHER": "secret",
                     "CODESAGE_OVERVIEW_CACHE": "0", "CODESAGE_COUPLING_RECURRENCE": "false",
                     "OMP_NUM_THREADS": "3"}
        daemon, indexer, report = wal.environments(inherited)
        self.assertEqual(daemon["CODESAGE_OVERVIEW_CACHE"], "1")
        self.assertEqual(indexer["CODESAGE_OVERVIEW_CACHE"], "0")
        for role in ("daemon", "indexer"):
            self.assertEqual(report[role]["CODESAGE_COUPLING_RECURRENCE"], "false")
            self.assertEqual(report[role]["CODESAGE_WATCH"], "0")
            self.assertEqual(report[role]["OMP_NUM_THREADS"], "3")
        self.assertNotIn("secret", json.dumps(report))
        self.assertEqual(inherited["CODESAGE_OVERVIEW_CACHE"], "0")

    def test_timeouts_reject_unbounded_or_nonpositive_values(self):
        for value in ("nan", "inf", "-inf", "0", "-1", "3601"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                wal.positive_seconds(value)
        self.assertEqual(wal.positive_seconds(".01"), .01)
        self.assertEqual(wal.positive_seconds("3600"), 3600)

    def test_only_running_analysis_counts_as_overlap(self):
        rows = [{"id": 1, "work_class": "analysis", "phase": "queued"},
                {"id": 2, "work_class": "native", "phase": "running"},
                {"id": 3, "work_class": "analysis", "phase": "running"}]
        self.assertEqual(wal.running_analysis({"active_executions": rows}), {3})


class ResponseTests(unittest.TestCase):
    def response(self, recomputed):
        value = {"project_root": "/scratch", "file_count": 3, "symbol_count": 4,
                 "feature_count": 0, "freshness": {},
                 **{key: [] for key in ("languages", "feature_summary", "top_risk_files",
                    "trust_boundary_clusters", "test_conventions", "entrypoints", "suggested_next_calls")}}
        if recomputed:
            value["_meta"] = {"ranking_recomputed": True}
        texts = [{"type": "text", "text": json.dumps(value)}]
        if recomputed:
            texts.append({"type": "text", "text": wal.RECOMPUTED_TEXT})
        return {"result": {"structuredContent": value, "content": texts}}

    def test_success_requires_observed_disclosure_when_requested(self):
        response = self.response(False)
        self.assertTrue(wal.response_contract(response)["valid"])
        self.assertFalse(wal.response_contract(response, True)["valid"])
        self.assertTrue(wal.response_contract(self.response(True), True)["valid"])

    def test_success_with_inconsistent_disclosure_is_rejected(self):
        response = self.response(True)
        response["result"]["content"].pop()
        self.assertFalse(wal.response_contract(response)["valid"])

    def test_success_with_different_text_payload_is_rejected(self):
        response = self.response(True)
        response["result"]["structuredContent"]["file_count"] = 999
        self.assertFalse(wal.response_contract(response)["valid"])

    def test_error_requires_real_incomplete_metadata_not_success_boolean(self):
        response = {"result": {"isError": True, "content": [{"type": "text", "text": json.dumps({
            "complete": False, "next": None, "phase": "running", "work_continuing": True})}]}}
        checked = wal.response_contract(response)
        self.assertFalse(checked["valid"])
        self.assertTrue(checked["incomplete_disclosed"])


class ProcessTests(unittest.IsolatedAsyncioTestCase):
    async def test_actual_response_is_retained_even_when_contract_fails(self):
        response = {"result": {"structuredContent": {}, "content": []}}
        future = asyncio.get_running_loop().create_future()
        future.set_result(response)
        client = SimpleNamespace(begin=AsyncMock(return_value=(1, future)), pending={1: future}, sequence=0)
        result = await wal.overview(client, Path("/scratch"), 1)
        self.assertEqual(result["response"], response)
        self.assertFalse(result["contract"]["valid"])
        self.assertEqual(client.pending, {})

    async def test_blocked_initial_send_is_bounded_and_unregisters_request(self):
        registered = asyncio.get_running_loop().create_future()
        entered = asyncio.Event()
        client = SimpleNamespace(sequence=0, pending={}, send=AsyncMock())
        async def begin(*args):
            client.sequence = 1
            client.pending[1] = registered
            entered.set()
            await asyncio.Event().wait()
        client.begin = begin
        result = await asyncio.wait_for(wal.overview(client, Path("/scratch"), .01), .5)
        self.assertTrue(entered.is_set())
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["cancellation"], "sent")
        self.assertEqual(client.pending, {})
        self.assertTrue(registered.cancelled())
        self.assertEqual(client.send.call_args.args[0]["params"]["requestId"], 1)

    async def test_blocked_cancel_notification_is_bounded(self):
        future = asyncio.get_running_loop().create_future()
        entered = asyncio.Event()
        async def send(message):
            entered.set()
            await asyncio.Event().wait()
        client = SimpleNamespace(sequence=0, pending={1: future}, send=send,
                                 begin=AsyncMock(return_value=(1, future)))
        result = await asyncio.wait_for(wal.overview(client, Path("/scratch"), .01), .5)
        self.assertTrue(entered.is_set())
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["cancellation"], "TimeoutError")
        self.assertEqual(client.pending, {})
        self.assertTrue(future.cancelled())

    async def test_cleanup_reaps_real_child(self):
        process = await asyncio.create_subprocess_exec(sys.executable, "-c", "import time; time.sleep(60)")
        await asyncio.wait_for(wal.stop(process), 7)
        self.assertIsNotNone(process.returncode)
        await wal.stop(process)

    async def test_log_is_bounded_without_stalling_child(self):
        with tempfile.TemporaryDirectory() as root:
            process = await asyncio.create_subprocess_exec(sys.executable, "-c",
                f"import sys; sys.stdout.buffer.write(b'x' * {wal.LOG_LIMIT + 123})",
                stdout=asyncio.subprocess.PIPE)
            try:
                path = Path(root) / "index.log"
                result = await asyncio.wait_for(wal.capture(process.stdout, path), 5)
                self.assertEqual(await process.wait(), 0)
                self.assertTrue(result["truncated"])
                self.assertEqual(result["bytes_received"], wal.LOG_LIMIT + 123)
                self.assertEqual(path.stat().st_size, wal.LOG_LIMIT)
            finally:
                await wal.stop(process)

    async def test_output_inside_original_is_rejected_before_validation(self):
        with tempfile.TemporaryDirectory() as root:
            args = SimpleNamespace(project=Path(root), output=Path(root) / "output",
                                   binary=Path(sys.executable), timeout=1, index_timeout=1)
            with patch.object(wal.runner, "verify_snapshot", side_effect=AssertionError("must not read")):
                with self.assertRaisesRegex(ValueError, "disjoint"):
                    await wal.run(args)

    async def test_existing_output_is_preserved(self):
        with tempfile.TemporaryDirectory() as root:
            project, output = Path(root) / "project", Path(root) / "output"
            project.mkdir()
            output.mkdir()
            canary = output / "canary"
            canary.write_text("preserve")
            args = SimpleNamespace(project=project, output=output, binary=Path(sys.executable),
                                   timeout=1, index_timeout=1)
            with patch.object(wal.runner, "verify_snapshot", return_value={}), \
                    patch.object(wal, "exercise", new=AsyncMock(side_effect=AssertionError("must not run"))):
                with self.assertRaises(FileExistsError):
                    await wal.run(args)
            self.assertEqual(canary.read_text(), "preserve")


if __name__ == "__main__":
    unittest.main()
