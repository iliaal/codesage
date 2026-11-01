"""Malformed CRG pages are visible misses, never partial retrieval hits."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

SCRIPT = Path(__file__).resolve().parents[2] / "bench/code-review-graph-bench-runner"


def load_runner():
    loader = importlib.machinery.SourceFileLoader("crg_bench_results", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def response(data):
    return {"result": {"content": [{"type": "text", "text": json.dumps(data)}]}}


class CrgBenchResultsTests(unittest.TestCase):
    def setUp(self):
        self.runner = load_runner()
        self.session = object.__new__(self.runner.CrgMcpSession)
        self.session._id = 0
        self.session._send = Mock()
        self.session._recv = Mock()

    def search(self, page):
        self.session._recv.return_value = page
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            paths = self.session.search("query", 10)
        return paths, stderr.getvalue()

    def test_valid_pages_preserve_paths_order_and_first_block(self):
        for paths in ([], ["gold.py"], ["gold.py", "gold.py", "other.py"],
                      [" ", "a\nb.py", "文.py", "/absolute.py", "0"]):
            for metadata in (False, True):
                with self.subTest(paths=paths, metadata=metadata):
                    page = response({"results": [{"file_path": p, "score": 0} for p in paths]})
                    if metadata:
                        page["result"]["isError"] = False
                        page["result"]["content"].append({"type": "image"})
                    actual, warning = self.search(page)
                    self.assertEqual(actual, list(dict.fromkeys(paths)))
                    self.assertEqual(warning, "")
        self.session._send.assert_called_with({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": {"name": "semantic_search_nodes_tool",
                       "arguments": {"query": "query", "limit": 10}},
        })

    def test_rejects_every_malformed_row_even_after_valid_prefix(self):
        bad_rows = [None, False, 3, "path", [], {}, {"other": "x"}]
        bad_rows += [{"file_path": value} for value in (None, False, True, 0, 3, 1.5, [], {}, "")]
        for bad in bad_rows:
            for rows in ([bad], [{"file_path": "gold.py"}, bad],
                         [{"file_path": "gold.py"}, {"file_path": "gold.py"}, bad]):
                with self.subTest(rows=rows):
                    paths, warning = self.search(response({"results": rows}))
                    self.assertEqual(paths, [])
                    self.assertIn("CRG search returned malformed results", warning)

    def test_rejects_malformed_payloads_envelopes_and_explicit_errors(self):
        pages = [None, False, 0, [], "bad", {}, {"result": None}, {"result": []}]
        pages += [{"result": {"content": value}} for value in (None, False, 0, {}, "x", [])]
        pages += [{"result": {"content": [value]}} for value in (None, False, 0, [], "x", {})]
        pages += [{"result": {"content": [{"text": value}]}} for value in (None, False, 0, [], {}, "{", "")]
        pages += [response(value) for value in (None, False, 0, [], "x", {})]
        pages += [response({"results": value}) for value in (None, False, 0, {}, "x")]
        for flag in (True, None, 0, 1, "false", []):
            page = response({"results": [{"file_path": "gold.py"}]})
            page["result"]["isError"] = flag
            pages.append(page)
        page = response({"results": [{"file_path": "gold.py"}]})
        page["error"] = {"message": "failed"}
        pages.append(page)
        for page in pages:
            with self.subTest(page=page):
                paths, warning = self.search(page)
                self.assertEqual(paths, [])
                self.assertIn("CRG search returned malformed results", warning)

    def test_malformed_outer_json_is_discarded(self):
        self.session._recv.side_effect = json.JSONDecodeError("bad", "{", 1)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(self.session.search("query", 10), [])
        self.assertIn("CRG search returned malformed results", stderr.getvalue())

    def test_warning_identifies_query_and_reason_on_one_line(self):
        for query in ("first", "second", "quote'\nnext\t文"):
            with self.subTest(query=query):
                self.session._recv.return_value = response({"results": [{}]})
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    self.session.search(query, 10)
                self.assertEqual(
                    stderr.getvalue(),
                    f"[warn] CRG search returned malformed results: query={query!r}; "
                    "reason=expected nonempty string file_path values\n",
                )

    def test_stream_correlates_initialize_and_search_responses(self):
        notification = {"jsonrpc": "2.0", "method": "notifications/message",
                        "params": {"level": "info", "data": "loading"}}
        pages = [notification, {"id": 1.0, "result": {}}]
        for request_id, path in ((2, "first.py"), (3, "second.py")):
            pages.extend([notification, {"id": request_id, **response({"results": [
                {"file_path": path}]})}])
        session = object.__new__(self.runner.CrgMcpSession)
        session._id = 0
        session._proc = Mock(stdin=io.BytesIO(), stdout=io.BytesIO(
            ("\n".join(json.dumps(page) for page in pages) + "\n").encode()))
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            session._init()
            self.assertEqual(session.search("first", 10), ["first.py"])
            self.assertEqual(session.search("second", 10), ["second.py"])
        self.assertEqual(stderr.getvalue(), "")
        requests = [json.loads(line) for line in session._proc.stdin.getvalue().splitlines()]
        self.assertEqual([request.get("id") for request in requests], [1, None, 2, 3])

    def test_stream_ignores_other_ids_and_preserves_matching_error(self):
        self.session._recv = self.runner.CrgMcpSession._recv.__get__(self.session)
        pages = [{"id": value, **response({"results": [{"file_path": "wrong.py"}]})}
                 for value in (None, True, "1", 0, 9)]
        pages.append({"id": 1, "error": {"code": -32603, "message": "failed"}})
        self.session._proc = Mock(stdout=io.BytesIO(
            ("\n".join(json.dumps(page) for page in pages) + "\n").encode()))
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(self.session.search("query", 10), [])
        self.assertIn("expected a successful response object", stderr.getvalue())

    def pipe_session(self, data, repeat=False):
        # Real pipes stay open after writing, so EOF cannot hide a blocked read.
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", """import sys, time
payload = bytes.fromhex(sys.argv[1])
if sys.argv[2] == 'repeat':
    while True:
        sys.stdout.buffer.write(payload)
        sys.stdout.buffer.flush()
        time.sleep(.01)
else:
    sys.stdout.buffer.write(payload)
    sys.stdout.buffer.flush()
    time.sleep(10)
""", data.hex(), "repeat" if repeat else "once"], stdout=subprocess.PIPE)

        def cleanup():
            proc.terminate()
            proc.wait(timeout=5)
            proc.stdout.close()

        self.addCleanup(cleanup)
        session = object.__new__(self.runner.CrgMcpSession)
        session._id = 1
        session._proc = proc
        return session

    def test_live_pipe_null_id_error_returns_without_waiting_for_eof(self):
        error = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "parse"}}
        session = self.pipe_session((json.dumps(error) + "\n").encode())
        self.assertEqual(session._recv(timeout=1), error)

    def test_live_pipe_skips_matching_id_server_request(self):
        request = {"id": 1, "method": "ping", "result": {"wrong": True}}
        reply = {"id": 1, "result": {"correct": True}}
        session = self.pipe_session((json.dumps(request) + "\n" + json.dumps(reply) + "\n").encode())
        self.assertEqual(session._recv(timeout=1), reply)

    def test_live_pipe_silence_and_partial_line_have_bounded_waits(self):
        for data in (b"", b'{"id": 1'):
            with self.subTest(data=data):
                session = self.pipe_session(data)
                start = time.monotonic()
                with self.assertRaisesRegex(ValueError, "timed out waiting"):
                    session._recv(timeout=.15)
                self.assertLess(time.monotonic() - start, 2)

    def test_live_pipe_notifications_do_not_restart_deadline(self):
        session = self.pipe_session(b'{"method":"notifications/message"}\n', repeat=True)
        start = time.monotonic()
        with self.assertRaisesRegex(ValueError, "timed out waiting"):
            session._recv(timeout=.15)
        self.assertLess(time.monotonic() - start, 2)

    def test_eof_remains_terminal_for_later_queries(self):
        session = object.__new__(self.runner.CrgMcpSession)
        session._id = 1
        session._proc = Mock(stdout=io.BytesIO())
        self.assertIsNone(session._recv(timeout=.15))
        self.assertIsNone(session._recv(timeout=.15))

    def test_initialization_failure_closes_process(self):
        process = Mock(stdin=io.BytesIO())
        with patch.object(self.runner.subprocess, "Popen", return_value=process), \
                patch.object(self.runner.CrgMcpSession, "_recv", side_effect=ValueError("timed out")):
            with self.assertRaisesRegex(ValueError, "timed out"):
                self.runner.CrgMcpSession(Path.cwd())
        process.terminate.assert_called_once_with()
        process.wait.assert_called_once_with(timeout=5)

    def test_cli_continues_after_malformed_page_without_partial_hit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "code-review-graph"
            binary.write_text(f"#!{sys.executable}\n" + '''import json
import sys
if "--version" in sys.argv:
    print("fixture")
    sys.exit(0)
for line in sys.stdin:
    request = json.loads(line)
    if request["method"] == "notifications/initialized":
        continue
    if request["method"] == "initialize":
        result = {}
    else:
        rows = [{"file_path": "gold.py"}]
        if request["params"]["arguments"]["query"] == "bad":
            rows.append({"file_path": []})
        result = {"content": [{"type": "text", "text": json.dumps({"results": rows})}]}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
''')
            binary.chmod(0o755)
            corpus = root / "cases.yaml"
            corpus.write_text(json.dumps({"project_root": str(root), "cases": [
                {"id": query, "query": query, "expected_files": ["gold.py"]}
                for query in ("bad", "good")
            ]}))
            proc = subprocess.run([sys.executable, str(SCRIPT), str(corpus)],
                                  env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"]},
                                  capture_output=True, text=True, timeout=20)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("| bad | ? | MISS | 0 | 0.00 | 0.00 |", proc.stdout)
            self.assertIn("| good | ? | 1 | 0 | 1.00 | 1.00 |", proc.stdout)
            self.assertIn("miss_rate=0.5000 median_first=1 r5=0.5000 r10=0.5000 cases=2", proc.stdout)
            self.assertEqual(proc.stderr.count("CRG search returned malformed results"), 1)
            self.assertNotIn("Traceback", proc.stderr)


if __name__ == "__main__":
    unittest.main()
