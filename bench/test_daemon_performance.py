"""Runner tests use real SQLite and Unix sockets; fake RPC results are not benchmarks."""

import asyncio
import importlib.util
import json
import sqlite3
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch
import os
from pathlib import Path
import io
from collections import Counter


spec = importlib.util.spec_from_file_location("daemon_performance", Path(__file__).parent / "daemon-performance/run.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class TimingArgumentTests(unittest.TestCase):
    def test_default_watcher_timeout_is_separate_from_request_timeout(self):
        argv = ["run.py", "run", "--binary", "/unused", "--project", "/unused", "--output", "/unused",
                "--source-commit", "test", "--build-features", "cuda"]
        with patch("sys.argv", argv), patch("sys.stdout", new=io.StringIO()), \
                patch.object(runner, "run", new=AsyncMock(return_value={"scenarios": []})) as run:
            runner.main()
        args = run.call_args.args[0]
        self.assertEqual(args.timeout, 30)
        self.assertEqual(args.watcher_timeout, 120)

    def test_all_timing_arguments_reject_nonfinite_and_nonpositive_values(self):
        base = ["run.py", "run", "--binary", "/unused", "--project", "/unused", "--output", "/unused",
                "--source-commit", "test", "--build-features", "cuda"]
        for flag in ("--timeout", "--watcher-timeout", "--abandon-after", "--observe"):
            for value in ("nan", "inf", "-inf", "0", "-1"):
                with self.subTest(flag=flag, value=value), patch("sys.argv", base + [f"{flag}={value}"]), \
                        patch("sys.stderr", new=io.StringIO()), patch("sys.stdout", new=io.StringIO()), \
                        patch.object(runner, "run", new=AsyncMock(return_value={"scenarios": []})) as run:
                    with self.assertRaises(SystemExit) as error:
                        runner.main()
                    self.assertEqual(error.exception.code, 2)
                    run.assert_not_called()


class MemoryTests(unittest.TestCase):
    def test_proc_status_reports_lifetime_highwater_separately_from_current_rss(self):
        with patch.object(Path, "read_text", return_value="Name:\tdaemon\nVmHWM:\t54321 kB\nVmRSS:\t12345 kB\n"):
            result = runner.process_memory(123)
        self.assertEqual(result, {"available": True, "daemon_lifetime_peak_rss_kib": 54321,
                                  "current_rss_kib": 12345})

    def test_missing_or_malformed_proc_memory_is_explicitly_unavailable(self):
        for content in ("", "VmHWM: 123 kB\n", "VmHWM: bad kB\nVmRSS: 12 kB\n",
                        "VmHWM: -1 kB\nVmRSS: 12 kB\n", "VmHWM: 12 MB\nVmRSS: 12 kB\n",
                        "VmHWM: 12 kB\nVmHWM: 13 kB\nVmRSS: 12 kB\n"):
            with self.subTest(content=content), patch.object(Path, "read_text", return_value=content):
                self.assertEqual(runner.process_memory(123), {"available": False, "error_type": "ValueError"})
        with patch.object(Path, "read_text", side_effect=FileNotFoundError()):
            self.assertEqual(runner.process_memory(123), {"available": False, "error_type": "FileNotFoundError"})


class SnapshotTests(unittest.TestCase):
    def test_online_backup_includes_uncheckpointed_wal(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            (source / ".codesage").mkdir(parents=True)
            (source / ".codesage/config.toml").write_text('[embedding]\ndevice="cpu"\n')
            (source / "subject.rs").write_text("pub fn subject() {}\n")
            with sqlite3.connect(source / ".codesage/index.db") as conn:
                conn.execute("PRAGMA journal_mode=WAL")
                conn.execute("PRAGMA wal_autocheckpoint=0")
                conn.execute("CREATE TABLE files(path TEXT)")
                conn.execute("CREATE TABLE symbols(name TEXT)")
                conn.execute("CREATE TABLE refs(name TEXT)")
                conn.execute("INSERT INTO files VALUES ('subject.rs')")
                conn.execute("INSERT INTO symbols VALUES ('wal_sentinel')")
                conn.commit()
                self.assertGreater((source / ".codesage/index.db-wal").stat().st_size, 0)
                output = Path(tmp) / "snapshot"
                result = runner.backup(source, output)
                with sqlite3.connect(output / ".codesage/index.db") as copied:
                    self.assertEqual(copied.execute("SELECT name FROM symbols").fetchall(), [("wal_sentinel",)])
                self.assertEqual((output / "subject.rs").read_text(), "pub fn subject() {}\n")
                self.assertEqual(result["counts"]["symbols"], 1)
                self.assertEqual(result["database_sha256"], runner.digest(output / ".codesage/index.db"))
                self.assertEqual(runner.verify_snapshot(output)["counts"]["symbols"], 1)
                (output / ".codesage/config.toml").write_text('[embedding]\ndevice="gpu"\n')
                with self.assertRaisesRegex(ValueError, "config_sha256 mismatch"):
                    runner.verify_snapshot(output)

    def test_database_mutation_is_rejected_before_measurement(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/index.db").write_bytes(b"original")
            (project / "snapshot.json").write_text(json.dumps({"database_sha256": runner.digest(project / ".codesage/index.db")}))
            (project / ".codesage/index.db").write_bytes(b"mutated")
            with self.assertRaisesRegex(ValueError, "database_sha256 mismatch"):
                runner.verify_snapshot(project)

    def test_existing_snapshot_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(FileExistsError):
                runner.backup(Path(tmp), Path(tmp))

    def test_post_run_source_mutation_marks_results_invalid_without_dropping_measurements(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            (source / ".codesage").mkdir(parents=True)
            (source / ".codesage/config.toml").write_text('[embedding]\ndevice="cpu"\n')
            (source / "subject.rs").write_text("pub fn subject() {}\n")
            with sqlite3.connect(source / ".codesage/index.db") as conn:
                conn.execute("CREATE TABLE files(path TEXT)")
                conn.execute("CREATE TABLE symbols(name TEXT)")
                conn.execute("CREATE TABLE refs(name TEXT)")
                conn.execute("INSERT INTO files VALUES ('subject.rs')")
            project = Path(tmp) / "snapshot"
            runner.backup(source, project)
            (project / "subject.rs").write_text("pub fn mutated() {}\n")
            result = {"scenarios": [{"scenario": "cold", "requests": [{"outcome": "success", "wall_s": 3.5}]}]}
            runner.record_input_validation(result, [project])
            self.assertEqual(result["input_validation"], "invalid_inputs")
            self.assertEqual(len(result["invalid_inputs"]), 1)
            self.assertEqual(result["scenarios"][0]["requests"][0]["wall_s"], 3.5)

    def test_post_run_nonempty_wal_invalidates_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / "snapshot.json").write_text("{}")
            with sqlite3.connect(project / ".codesage/index.db") as conn:
                conn.execute("PRAGMA journal_mode=WAL")
                conn.execute("PRAGMA wal_autocheckpoint=0")
                conn.execute("CREATE TABLE committed(value TEXT)")
                conn.execute("INSERT INTO committed VALUES ('changed during run')")
                conn.commit()
                with self.assertRaisesRegex(ValueError, "nonempty WAL"):
                    runner.verify_snapshot(project)
                result = {"scenarios": [{"scenario": "warm", "summary": {"successes": 20}}]}
                runner.record_input_validation(result, [project])
                self.assertEqual(result["input_validation"], "invalid_inputs")
                self.assertFalse(result["input_after_run"][0]["valid"])
                self.assertEqual(result["scenarios"][0]["summary"]["successes"], 20)


class ResultTests(unittest.TestCase):
    def test_parity_preserves_coverage_truncation_and_static_suggestions(self):
        base = {"result": {"structuredContent": {"_meta": {"coverage": {"files": 1}, "truncated": False,
                "stale_files": ["a.rs"], "stale_warning": "changed"},
                "suggested_next_calls": [{"tool": "search", "why": "static reason"}]}}}
        original = runner.stable_result(base)
        base["result"]["structuredContent"]["_meta"]["stale_files"] = ["b.rs"]
        self.assertEqual(original, runner.stable_result(base))
        base["result"]["structuredContent"]["_meta"]["truncated"] = True
        self.assertNotEqual(original, runner.stable_result(base))
        base["result"]["structuredContent"]["_meta"]["truncated"] = False
        base["result"]["structuredContent"]["_meta"]["coverage"]["files"] = 2
        self.assertNotEqual(original, runner.stable_result(base))
        base["result"]["structuredContent"]["_meta"]["coverage"]["files"] = 1
        base["result"]["structuredContent"]["suggested_next_calls"][0]["why"] = "changed reason"
        self.assertNotEqual(original, runner.stable_result(base))

    def test_secret_environment_is_rejected_without_echoing_value(self):
        sentinel = "forbidden-secret-unique-72cf"
        with self.assertRaises(ValueError) as caught:
            runner.controlled_environment([f"HF_TOKEN={sentinel}"])
        self.assertNotIn(sentinel, str(caught.exception))
        with patch.dict(os.environ, {"HF_TOKEN": sentinel, "OMP_NUM_THREADS": "3"}, clear=True):
            _overrides, recorded = runner.controlled_environment([])
        self.assertEqual(recorded, {"OMP_NUM_THREADS": "3"})


class ModelScenarioTests(unittest.IsolatedAsyncioTestCase):
    async def test_failed_cold_request_does_not_run_or_label_warm_calls(self):
        failed = {"scenario": "cold", "summary": {"successes": 0, "client_timeout": 1}}
        with patch.object(runner, "measure", new=AsyncMock(return_value=failed)) as measure:
            rows = await runner.measure_cold_warm(None, ("project_overview", {}), 1)
        self.assertEqual(measure.await_count, 1)
        self.assertEqual(rows[1], {"scenario": "warm", "skipped": "cold request did not complete successfully"})

    async def test_failed_initialization_does_not_run_or_label_warm_calls(self):
        failed = {"scenario": "model_initialization", "summary": {"successes": 0}}
        with patch.object(runner, "measure", new=AsyncMock(return_value=failed)) as measure:
            rows = await runner.measure_models(None, ("search", {}), 1)
        self.assertEqual(measure.await_count, 1)
        self.assertEqual(rows[1], {"scenario": "model_warm", "skipped": "initialization did not complete successfully"})


class LaunchAndWatcherTests(unittest.IsolatedAsyncioTestCase):
    async def test_overlapping_output_is_rejected_before_creating_directories(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            project = root / "project"
            project.mkdir()
            alias = root / "alias"
            alias.symlink_to(project, target_is_directory=True)
            for output in (project, project / "new-run", root, alias / "new-run"):
                with self.subTest(output=output):
                    existed = output.exists()
                    args = SimpleNamespace(binary=root / "absent-binary", project=[project], output=output)
                    with self.assertRaisesRegex(ValueError, "must be disjoint"):
                        await runner.run(args)
                    self.assertEqual(output.exists(), existed)

    async def test_sibling_output_passes_overlap_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            project = root / "project"
            project.mkdir()
            output = root / "project-results"
            args = SimpleNamespace(binary=root / "absent-binary", project=[project], output=output)
            with self.assertRaises(FileNotFoundError):
                await runner.run(args)
            self.assertTrue(output.is_dir())

    async def test_watcher_default_completion_budget_does_not_change_mcp_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)) as measure, \
                    patch.object(runner, "watcher_status", return_value={"pid": 1}), \
                    patch.object(runner, "wait_watcher_quiescent", new=AsyncMock(return_value={"quiescent": False})) as wait:
                await runner.measure_watcher(None, project, 30, 5)
            self.assertEqual(measure.call_args.args[3], 30)
            self.assertEqual(wait.call_args.args[2], 120)

    async def test_replaced_binary_is_rejected_against_frozen_manifest_before_launch(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "codesage"
            binary.write_bytes(b"original manifest executable")
            frozen = runner.digest(binary)
            daemon = runner.Daemon(binary, Path(tmp) / "runtime", False, {}, frozen)
            binary.write_bytes(b"replacement executable")
            with self.assertRaisesRegex(ValueError, "frozen manifest hash"):
                await daemon.__aenter__()
            self.assertFalse((Path(tmp) / "runtime").exists())

    async def test_failed_watcher_setup_is_retained_without_watcher_measurements(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            setup = {"scenario": "watcher_start", "summary": {"successes": 0, "client_timeout": 1}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                rows = await runner.measure_watcher(None, project, .01, .01)
            self.assertEqual(rows[0], setup)
            self.assertEqual([row["unavailable"] for row in rows[1:]], [
                "setup request did not complete successfully", "setup request did not complete successfully"])
            self.assertFalse((project / "daemon_performance_watcher_probe.rs").exists())

    async def test_disabled_watcher_cannot_be_reported_active_after_successful_setup(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("[index]\nwatch=false\n")
            setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                rows = await runner.measure_watcher(None, project, .01, .01)
            self.assertEqual(rows[1]["unavailable"], "project watcher disabled")
            self.assertEqual(rows[2]["unavailable"], "project watcher disabled")

    async def test_other_process_watcher_status_does_not_prove_measured_daemon_activation(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            (project / ".codesage/watch.status").write_text(json.dumps({"pid": os.getpid() + 1}))
            daemon = SimpleNamespace(process=SimpleNamespace(pid=os.getpid(), returncode=None))
            setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                rows = await runner.measure_watcher(daemon, project, .01, .01, watcher_timeout=.01)
            self.assertEqual(rows[1]["unavailable"], "no active watcher status owned by measured daemon")

    async def test_pending_startup_cannot_be_measured_as_idle(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            (project / ".codesage/watch.status").write_text(json.dumps({
                "pid": os.getpid(), "startup_reconciled": False,
                "reconciliation_pending": True, "reconciliation_parked": False, "stale_parked": 0}))
            daemon = SimpleNamespace(process=SimpleNamespace(pid=os.getpid(), returncode=None))
            setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                rows = await runner.measure_watcher(daemon, project, .01, .01, watcher_timeout=.01)
            self.assertFalse(setup["startup_quiescence"]["quiescent"])
            self.assertIn("unavailable", rows[1])
            self.assertNotIn("process_cpu_s", rows[1])
            self.assertFalse((project / "daemon_performance_watcher_probe.rs").exists())

    async def test_live_startup_status_transition_enables_idle_measurement(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            status_path = project / ".codesage/watch.status"
            status = {"pid": os.getpid(), "startup_reconciled": False,
                      "reconciliation_pending": True, "reconciliation_parked": False, "stale_parked": 0}
            status_path.write_text(json.dumps(status))
            daemon = SimpleNamespace(process=SimpleNamespace(pid=os.getpid(), returncode=None))
            async def finish_startup():
                await asyncio.sleep(.05)
                status.update(startup_reconciled=True, reconciliation_pending=False)
                status_path.write_text(json.dumps(status))
            setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
            async with asyncio.TaskGroup() as group:
                group.create_task(finish_startup())
                with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                    rows = await runner.measure_watcher(daemon, project, 2, .01, watcher_timeout=2)
            self.assertTrue(setup["startup_quiescence"]["quiescent"])
            self.assertGreaterEqual(setup["startup_quiescence"]["wall_s"], .5)
            self.assertEqual(rows[1]["scenario"], "watcher_idle")
            self.assertTrue(rows[1]["measurement_complete"])
            self.assertEqual(rows[2]["scenario"], "watcher_active")
            self.assertFalse(rows[2]["measurement_complete"], "a live watcher without an indexed probe is incomplete")
            self.assertFalse((project / "daemon_performance_watcher_probe.rs").exists())

    async def test_default_watcher_setup_uses_semantic_search(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / ".codesage").mkdir()
            (project / ".codesage/config.toml").write_text("")
            setup = {"scenario": "watcher_start", "summary": {"successes": 0}}
            with patch.object(runner, "measure", new=AsyncMock(return_value=setup)) as measure:
                await runner.measure_watcher(None, project, .01, .01)
            self.assertEqual(measure.call_args.args[2][0][0], "search")

    async def test_probe_completion_requires_matching_model_hashes_and_drained_watcher(self):
        for outcome in ("complete", "structural_only", "wrong_model", "wrong_hash", "pending"):
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory() as tmp:
                project = Path(tmp)
                meta = project / ".codesage"
                meta.mkdir()
                (meta / "config.toml").write_text('[embedding]\nmodel="test/model"\n')
                status = {"pid": os.getpid(), "startup_reconciled": True,
                          "reconciliation_pending": False, "reconciliation_parked": False, "stale_parked": 0}
                (meta / "watch.status").write_text(json.dumps(status))
                with sqlite3.connect(meta / "index.db") as db:
                    db.executescript("CREATE TABLE files (path TEXT, content_hash TEXT);"
                                     "CREATE TABLE semantic_files (chunk_table TEXT, path TEXT, content_hash TEXT);"
                                     "CREATE TABLE semantic_models (chunk_table TEXT, model TEXT);")
                    db.execute("INSERT INTO semantic_models VALUES ('chunks_test', ?)",
                               ("other/model" if outcome == "wrong_model" else "test/model",))
                daemon = SimpleNamespace(process=SimpleNamespace(pid=os.getpid(), returncode=None))
                setup = {"scenario": "watcher_start", "summary": {"successes": 1}}
                probe = project / "daemon_performance_watcher_probe.rs"

                async def index_probe():
                    while not probe.exists():
                        await asyncio.sleep(.01)
                    status["reconciliation_pending"] = True
                    (meta / "watch.status").write_text(json.dumps(status))
                    with sqlite3.connect(meta / "index.db") as db:
                        db.execute("INSERT INTO files VALUES (?, ?)", (probe.name, runner.digest(probe)))
                        if outcome != "structural_only":
                            db.execute("INSERT INTO semantic_files VALUES ('chunks_test', ?, ?)",
                                       (probe.name, "stale" if outcome == "wrong_hash" else runner.digest(probe)))
                    await asyncio.sleep(.1)
                    if outcome != "pending":
                        status["reconciliation_pending"] = False
                        (meta / "watch.status").write_text(json.dumps(status))

                async with asyncio.TaskGroup() as group:
                    group.create_task(index_probe())
                    with patch.object(runner, "measure", new=AsyncMock(return_value=setup)):
                        rows = await runner.measure_watcher(daemon, project, .01, .01, watcher_timeout=1)
                active = rows[-1]
                self.assertEqual(active["measurement_complete"], outcome == "complete")
                self.assertFalse(probe.exists())
                if outcome == "complete":
                    self.assertGreaterEqual(active["wall_s"], .5)
                    self.assertTrue(active["indexed_probe"]["structural"])
                    self.assertTrue(active["indexed_probe"]["semantic"])

    async def test_legacy_status_cannot_attest_startup_quiescence(self):
        status = {"reconciliation_pending": False, "reconciliation_parked": False, "stale_parked": 0}
        self.assertFalse(runner.watcher_quiescent(status))
        status["startup_reconciled"] = True
        self.assertTrue(runner.watcher_quiescent(status))
        status["stale_parked"] = 1
        self.assertFalse(runner.watcher_quiescent(status))


class SemanticResultTests(unittest.TestCase):

    def test_live_summary_changes_do_not_hide_score_changes(self):
        def response(score, summary):
            return {"result": {"structuredContent": {
                "top_risk_files": [{"file": "a.rs", "score": score}],
                "freshness": {"structural_summary": summary, "semantic_indexed_files": 7}}}}
        a = response(.7, "just now")
        b = response(.7, "one minute ago")
        c = response(.8, "just now")
        self.assertEqual(runner.stable_result(a), runner.stable_result(b))
        self.assertNotEqual(runner.stable_result(a), runner.stable_result(c))
        b["result"]["structuredContent"]["freshness"]["semantic_indexed_files"] = 8
        self.assertNotEqual(runner.stable_result(a), runner.stable_result(b))

    def test_timeouts_do_not_improve_successful_latency(self):
        rows = [{"outcome": "success", "wall_s": 4}, {"outcome": "client_timeout", "wall_s": .01}]
        result = runner.summary(rows)
        self.assertEqual(result["successes"], 1)
        self.assertEqual(result["client_timeout"], 1)
        self.assertEqual(result["p50_s"], 4)
        self.assertIsNone(result["p95_s"])
        self.assertIsNone(result["p99_s"])


class TransportTests(unittest.IsolatedAsyncioTestCase):
    async def test_request_deadline_includes_blocked_send(self):
        async def blocked_send(value):
            await asyncio.Event().wait()
        with patch.object(self.client, "send", new=blocked_send):
            request = asyncio.create_task(self.client.request("ping", {}, .02))
            done, _ = await asyncio.wait({request}, timeout=.2)
            if not done:
                request.cancel()
            with self.assertRaises((TimeoutError, asyncio.CancelledError)):
                await request
        self.assertTrue(done, "request send exceeded its deadline")
        self.assertEqual(self.client.pending, {})

    async def test_call_deadline_includes_blocked_send(self):
        async def blocked_send(value):
            await asyncio.Event().wait()
        with patch.object(self.client, "send", new=blocked_send):
            result = await asyncio.wait_for(runner.call(self.client, "project_overview", {}, .02), .2)
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(self.client.pending, {})

    async def test_cancel_notification_has_bounded_delivery(self):
        original_send = self.client.send
        async def blocked_cancel(value):
            if value.get("method") == "notifications/cancelled":
                await asyncio.Event().wait()
            await original_send(value)
        with patch.object(self.client, "send", new=blocked_cancel), \
                patch.object(runner, "TRANSPORT_CLEANUP_TIMEOUT", .02, create=True):
            result = await asyncio.wait_for(runner.call(self.client, "project_overview", {}, .02, "cancel"), .2)
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["abandon_action"], "cancel_send_failed")
        self.assertEqual(result["abandon_error_type"], "TimeoutError")
        self.assertEqual(self.client.pending, {})

    async def test_close_aborts_blocked_writer_and_stops_reader(self):
        async def blocked_close():
            await asyncio.Event().wait()
        with patch.object(self.client.writer, "wait_closed", new=blocked_close), \
                patch.object(self.client.writer.transport, "abort", wraps=self.client.writer.transport.abort) as abort, \
                patch.object(runner, "TRANSPORT_CLEANUP_TIMEOUT", .02, create=True):
            await asyncio.wait_for(self.client.close(), .2)
        abort.assert_called_once()
        self.assertTrue(self.client.reader_task.done())

    async def test_repeated_close_does_not_reawait_cancelled_protocol_waiter(self):
        closed = asyncio.get_running_loop().create_future()
        async def shared_close():
            await closed
        with patch.object(self.client.writer, "wait_closed", new=shared_close), \
                patch.object(runner, "TRANSPORT_CLEANUP_TIMEOUT", .02):
            await self.client.close()
            self.assertTrue(closed.cancelled())
            await self.client.close()

    async def test_initialized_notification_deadline_closes_connection(self):
        original_send = runner.Client.send
        async def blocked_initialized(client, value):
            if value.get("method") == "notifications/initialized":
                await asyncio.Event().wait()
            await original_send(client, value)
        with patch.object(runner.Client, "send", new=blocked_initialized), \
                patch.object(runner, "TRANSPORT_CONNECT_TIMEOUT", .02, create=True):
            task = asyncio.create_task(runner.Client.connect(self.socket))
            done, _ = await asyncio.wait({task}, timeout=.2)
            if not done:
                task.cancel()
            with self.assertRaises((TimeoutError, asyncio.CancelledError)):
                await task
        self.assertTrue(done, "initialized notification exceeded connection deadline")
        await asyncio.sleep(.01)
        self.assertTrue(self.connections[-1].is_closing())

    async def test_missing_response_result_is_never_success(self):
        async def malformed(message, writer):
            writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"]}).encode() + b"\n")
            await writer.drain()
        self.tool_handler = malformed
        result = await runner.call(self.client, "project_overview", {}, .1)
        self.assertEqual(result["outcome"], "transport_error")
        self.assertNotIn("stable_sha256", result)

    async def test_cancelled_send_does_not_remove_other_request(self):
        original_send = self.client.send
        blocked = asyncio.Event()
        other_arrived, release = asyncio.Event(), asyncio.Event()
        async def handler(message, writer):
            other_arrived.set()
            await release.wait()
            return {"structuredContent": {"file_count": 7}}
        self.tool_handler = handler
        async def selective_send(value):
            if value.get("params", {}).get("blocked"):
                blocked.set()
                await asyncio.Event().wait()
            await original_send(value)
        with patch.object(self.client, "send", new=selective_send):
            request = asyncio.create_task(self.client.request("ping", {"blocked": True}, 1))
            await blocked.wait()
            other = asyncio.create_task(runner.call(self.client, "project_overview", {}, .5))
            try:
                await asyncio.wait_for(other_arrived.wait(), .5)
                request.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await request
            finally:
                release.set()
            result = await other
        self.assertEqual(result["outcome"], "success")
        self.assertEqual(self.client.pending, {})

    async def test_begin_cleans_pending_request_when_send_is_cancelled(self):
        entered = asyncio.Event()
        async def blocked_send(value):
            entered.set()
            await asyncio.Event().wait()
        with patch.object(self.client, "send", new=blocked_send):
            task = asyncio.create_task(self.client.begin("ping", {}))
            await entered.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(self.client.pending, {})

    async def test_disconnect_with_blocked_writer_close_still_returns_timeout_row(self):
        async def blocked_close():
            await asyncio.Event().wait()
        with patch.object(self.client.writer, "wait_closed", new=blocked_close), \
                patch.object(runner, "TRANSPORT_CLEANUP_TIMEOUT", .02):
            result = await asyncio.wait_for(runner.call(self.client, "project_overview", {}, .02, "disconnect"), .2)
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["abandon_action"], "transport_closed")
        self.assertTrue(self.client.reader_task.done())

    async def test_valid_notification_and_protocol_error_remain_supported(self):
        async def valid_error(message, writer):
            for value in ({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}},
                          {"jsonrpc": "2.0", "id": message["id"],
                           "error": {"code": -32602, "message": "controlled invalid params"}}):
                writer.write(json.dumps(value).encode() + b"\n")
            await writer.drain()
        self.tool_handler = valid_error
        result = await runner.call(self.client, "project_overview", {}, .1)
        self.assertEqual(result["outcome"], "protocol_error")
        self.assertIsNone(result["stable_sha256"])

    async def test_invalid_response_envelopes_fail_closed(self):
        for payload in ({"result": {}, "error": {"code": 1, "message": "both"}},
                        {"result": []}, {"error": {}}, {"result": {}, "jsonrpc": "1.0"}):
            with self.subTest(payload=payload):
                async def malformed(message, writer):
                    writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], **payload}).encode() + b"\n")
                    await writer.drain()
                self.tool_handler = malformed
                client = await runner.Client.connect(self.socket)
                try:
                    result = await runner.call(client, "project_overview", {}, .1)
                    self.assertEqual(result["outcome"], "transport_error")
                finally:
                    await client.close()

    async def test_invalid_tool_result_is_not_a_success_or_stats_snapshot(self):
        for payload in ({}, {"content": {}}, {"content": [None]},
                        {"content": [], "isError": "false"}, {"content": [], "structuredContent": []}):
            with self.subTest(payload=payload):
                async def malformed(message, writer):
                    writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": payload}).encode() + b"\n")
                    await writer.drain()
                self.tool_handler = malformed
                result = await runner.call(self.client, "project_overview", {}, .1)
                self.assertEqual(result["outcome"], "transport_error")
                with self.assertRaisesRegex(ValueError, "invalid MCP tool result"):
                    await runner.stats(self.client)

    async def test_empty_content_is_a_valid_tool_result(self):
        async def empty(message, writer):
            return {"content": []}
        self.tool_handler = empty
        result = await runner.call(self.client, "project_overview", {}, .1)
        self.assertEqual(result["outcome"], "success")

    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="csp-test-")
        self.messages = []
        self.connections = []
        self.tasks = set()
        self.reject_connection = None
        self.close_tool = False
        self.respond_tool = False
        self.tool_handler = None
        async def serve(reader, writer):
            self.connections.append(writer)
            number = len(self.connections)
            task = asyncio.current_task()
            self.tasks.add(task)
            try:
                while line := await reader.readline():
                    message = json.loads(line)
                    self.messages.append(message)
                    if message["method"] == "initialize":
                        if number == self.reject_connection:
                            return
                        writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": {}}).encode() + b"\n")
                        await writer.drain()
                    elif message["method"] == "tools/call" and self.tool_handler is not None:
                        response = await self.tool_handler(message, writer)
                        if response is not None:
                            writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                                                     "result": {"content": [], **response}}).encode() + b"\n")
                            await writer.drain()
                    elif message["method"] == "tools/call" and self.close_tool:
                        return
                    elif message["method"] == "tools/call" and self.respond_tool:
                        writer.write(json.dumps({"jsonrpc": "2.0", "id": message["id"],
                                                 "result": {"content": [], "structuredContent": {"file_count": 7}}}).encode() + b"\n")
                        await writer.drain()
            finally:
                self.tasks.remove(task)
                writer.close()
        self.socket = Path(self.tmp.name) / "test.sock"
        self.server = await asyncio.start_unix_server(serve, self.socket)
        self.client = await runner.Client.connect(self.socket)

    async def asyncTearDown(self):
        await self.client.close()
        self.server.close()
        await self.server.wait_closed()
        for writer in self.connections:
            writer.close()
        await asyncio.gather(*self.tasks)
        self.tmp.cleanup()

    async def test_protocol_cancellation_targets_the_timed_out_request(self):
        result = await runner.call(self.client, "project_overview", {"project": "/fixture"}, .01, "cancel")
        await asyncio.sleep(.01)
        request = next(row for row in self.messages if row["method"] == "tools/call")
        notification = next(row for row in self.messages if row["method"] == "notifications/cancelled")
        self.assertEqual(notification["params"]["requestId"], request["id"])
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["abandon_action"], "cancel_sent")

    async def test_local_timeout_keeps_transport_open_without_protocol_cancel(self):
        result = await runner.call(self.client, "project_overview", {}, .01)
        await self.client.request("initialize", {}, 1)
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual([row["method"] for row in self.messages], [
            "initialize", "notifications/initialized", "tools/call", "initialize"])

    async def test_disconnect_closes_transport(self):
        result = await runner.call(self.client, "project_overview", {}, .01, "disconnect")
        self.assertEqual(result["outcome"], "client_timeout")
        self.assertEqual(result["abandon_action"], "transport_closed")
        self.assertTrue(self.client.writer.is_closing())

    async def test_partial_handshake_failure_closes_earlier_connections(self):
        self.reject_connection = 3
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            result = await runner.measure(daemon, "partial", [("project_overview", {})] * 2, .1, True)
        await asyncio.sleep(.01)
        self.assertFalse(result["measurement_complete"])
        self.assertEqual(result["outcome"], "setup_or_observation_error")
        self.assertTrue(self.connections[1].is_closing())

    async def test_server_disconnects_are_counted_without_orphaned_calls(self):
        self.close_tool = True
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            result = await runner.measure(daemon, "closed", [("project_overview", {"project": "fixture"})] * 2, .1, True)
        self.assertEqual(result["summary"]["transport_error"], 2)
        self.assertEqual(result["summary"]["successes"], 0)

    async def test_unavailable_memory_preserves_completed_requests_and_cpu(self):
        self.respond_tool = True
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        original_read = Path.read_text
        def read(path, *args, **kwargs):
            if str(path) == f"/proc/{os.getpid()}/status":
                raise FileNotFoundError()
            return original_read(path, *args, **kwargs)
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})), \
                patch.object(Path, "read_text", new=read):
            result = await runner.measure(daemon, "memory_missing", [("project_overview", {})], 1)
        self.assertTrue(result["measurement_complete"])
        self.assertEqual(result["summary"]["successes"], 1)
        self.assertGreaterEqual(result["process_cpu_to_responses_s"], 0)
        self.assertEqual(result["memory_before"], {"available": False, "error_type": "FileNotFoundError"})
        self.assertEqual(result["memory_after"], result["memory_before"])

    async def test_model_burst_initializes_before_overlapping_searches_and_other_project_probe(self):
        initialized = False
        arrived = 0
        burst_started = asyncio.Event()
        async def handler(message, writer):
            nonlocal initialized, arrived
            if not initialized:
                self.assertEqual(message["params"]["name"], "search")
                initialized = True
            else:
                arrived += 1
                if arrived == 5:
                    burst_started.set()
                await burst_started.wait()
            return {"structuredContent": {"results": []}}
        self.tool_handler = handler
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        search = ("search", {"project": "/primary", "query": "private query", "limit": 5})
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            rows = await runner.measure_model_concurrent(daemon, search, 2, 4, Path("/other"), "needle", .01)
        self.assertEqual([row["scenario"] for row in rows], ["model_concurrent_initialization", "model_concurrent"])
        self.assertEqual(rows[0]["summary"]["successes"], 1)
        self.assertEqual(rows[1]["summary"]["successes"], 5)
        self.assertEqual(rows[1]["by_project"]["/primary"]["successes"], 4)
        self.assertEqual(rows[1]["by_project"]["/other"]["successes"], 1)
        calls = [message["params"] for message in self.messages if message["method"] == "tools/call"]
        self.assertEqual(Counter(call["name"] for call in calls), {"search": 5, "find_symbol": 1})
        self.assertEqual(next(call for call in calls if call["name"] == "find_symbol"),
                         {"name": "find_symbol", "arguments": {"project": "/other", "name": "needle"}})
        self.assertNotIn("private query", json.dumps(rows))
        self.assertEqual(rows[1]["post_response_observation_s"], .01)
        self.assertTrue(rows[1]["measurement_complete"])

    async def test_failed_model_initialization_skips_burst(self):
        async def handler(message, writer):
            return {"isError": True, "content": [{"type": "text", "text": "initialization failed"}]}
        self.tool_handler = handler
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            rows = await runner.measure_model_concurrent(daemon, ("search", {"project": "/primary"}),
                                                         1, 4, None, "needle", 0)
        self.assertEqual(rows[0]["summary"]["tool_error"], 1)
        self.assertEqual(rows[1]["skipped"], "initialization did not complete successfully")
        calls = [message for message in self.messages if message["method"] == "tools/call"]
        self.assertEqual(len(calls), 1)

    async def test_failed_final_stats_preserves_completed_requests_and_cpu(self):
        self.respond_tool = True
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(side_effect=[{"available": False}, TimeoutError()])):
            result = await runner.measure(daemon, "observed", [("project_overview", {"project": "fixture"})] * 2, .1, True)
        self.assertFalse(result["measurement_complete"])
        self.assertEqual(result["failed_phase"], "observation")
        self.assertEqual(result["summary"]["successes"], 2)
        self.assertEqual(len(result["requests"]), 2)
        self.assertGreaterEqual(result["process_cpu_to_responses_s"], 0)
        self.assertGreaterEqual(result["process_cpu_after_responses_s"], 0)

    async def test_failed_cancel_notification_retains_timeout_rows_in_concurrent_scenario(self):
        original_send = runner.Client.send
        async def fail_cancellation(client, value):
            if value.get("method") == "notifications/cancelled":
                raise BrokenPipeError("controlled cancellation write failure")
            await original_send(client, value)
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            with patch.object(runner.Client, "send", new=fail_cancellation):
                result = await runner.measure(daemon, "cancel", [("project_overview", {"project": "fixture"})] * 2,
                                              .01, True, "cancel")
        self.assertEqual(result["summary"]["client_timeout"], 2)
        self.assertEqual([row["abandon_action"] for row in result["requests"]], ["cancel_send_failed"] * 2)
        self.assertTrue(result["measurement_complete"])

    async def test_bounded_review_preserves_all_calls_and_successful_session(self):
        self.respond_tool = True
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            result = await runner.measure_review(daemon, Path("/fixture"), "unique_symbol", 1, 4)
        self.assertTrue(result["measurement_complete"])
        self.assertEqual(result["scenario"], "review_bounded")
        self.assertEqual(result["summary"]["successes"], 62)
        self.assertEqual(Counter(row["tool"] for row in result["requests"]),
                         {"project_overview": 23, "find_references": 38, "session_start": 1})
        self.assertEqual(result["requests"][-1]["outcome"], "success")
        self.assertTrue(all(row["stable_sha256"] for row in result["requests"]))
        self.assertTrue(all(row["client_scheduling_wait_s"] >= 0 for row in result["requests"]))
        self.assertIn("bounded client scheduling substitutes unavailable original timing", result["substitution"])
        self.assertGreaterEqual(result["process_cpu_to_responses_s"], 0)
        self.assertGreater(result["wall_to_responses_s"], 0)
        reference_calls = [row for row in self.messages if row.get("params", {}).get("name") == "find_references"]
        self.assertEqual(len(reference_calls), 38)
        self.assertTrue(all(row["params"]["arguments"]["name"] == "unique_symbol" for row in reference_calls))

    async def test_bounded_calls_overlap_without_exceeding_limit(self):
        active = peak = 0
        at_limit, exceeded, release = asyncio.Event(), asyncio.Event(), asyncio.Event()
        async def handler(message, writer):
            nonlocal active, peak
            active += 1
            peak = max(peak, active)
            if active == 4:
                at_limit.set()
            if active > 4:
                exceeded.set()
            try:
                await release.wait()
                return {"structuredContent": {"file_count": 7}}
            finally:
                active -= 1
        self.tool_handler = handler
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            task = asyncio.create_task(runner.measure(daemon, "bounded", [("project_overview", {})] * 8,
                                                     2, True, max_inflight=4))
            try:
                await asyncio.wait_for(at_limit.wait(), 2)
                try:
                    await asyncio.wait_for(exceeded.wait(), .05)
                except TimeoutError:
                    pass
            finally:
                release.set()
                result = await task
        self.assertEqual(peak, 4)
        self.assertEqual(active, 0)
        self.assertEqual(result["summary"]["successes"], 8)

    async def test_unbounded_review_still_starts_all_62_calls_together(self):
        arrived = 0
        all_started = asyncio.Event()
        async def handler(message, writer):
            nonlocal arrived
            arrived += 1
            if arrived == 62:
                all_started.set()
            await all_started.wait()
            return {"structuredContent": {"file_count": 7}}
        self.tool_handler = handler
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            result = await runner.measure_review(daemon, Path("/fixture"), "symbol", 2)
        self.assertEqual(result["scenario"], "review")
        self.assertEqual(result["summary"]["successes"], 62)
        self.assertNotIn("client_scheduling", result)
        self.assertTrue(all("client_scheduling_wait_s" not in row for row in result["requests"]))
        self.assertIn("concurrent burst substitutes unavailable original timing", result["substitution"])

    async def test_bounded_failures_release_permits_and_retain_every_row(self):
        async def handler(message, writer):
            index = message["params"]["arguments"]["index"]
            if index == 0:
                writer.close()
                return None
            if index == 1:
                return None
            if index == 2:
                return {"isError": True, "content": [{"type": "text", "text": "controlled tool error"}]}
            return {"structuredContent": {"file_count": 7}}
        self.tool_handler = handler
        daemon = SimpleNamespace(client=self.client, socket=self.socket, process=SimpleNamespace(pid=os.getpid()))
        with patch.object(runner, "stats", new=AsyncMock(return_value={"available": False})):
            result = await asyncio.wait_for(runner.measure(daemon, "bounded_failures",
                [("project_overview", {"index": index}) for index in range(6)], .05, True, max_inflight=1), 2)
        self.assertTrue(result["measurement_complete"])
        self.assertEqual([row["outcome"] for row in result["requests"]],
                         ["transport_error", "client_timeout", "tool_error", "success", "success", "success"])


if __name__ == "__main__":
    unittest.main()
