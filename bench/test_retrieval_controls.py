#!/usr/bin/env python3
"""Control integration uses real SQLite, source bytes, and ripgrep; no model inference."""
from __future__ import annotations

import contextlib
import copy
import hashlib
import importlib.machinery
import importlib.util
import io
import json
import os
import shlex
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import _retrieval_controls as controls


def load(filename, name):
    spec = importlib.util.spec_from_loader(name, importlib.machinery.SourceFileLoader(
        name, str(Path(__file__).with_name(filename))))
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


runner = load("codesage-bench-runner", "control_runner")
compare = load("compare-runs.py", "control_compare")
self_eval = load("self-eval.py", "control_self_eval")


def real_rg():
    binary = shutil.which("rg")
    if binary is None:
        raise RuntimeError("retrieval-control tests require ripgrep (rg) on PATH")
    return str(Path(binary).resolve())


def fixture_runtime():
    stderr = "embedding model loaded\n"
    return {**controls.QUERY_RUNTIME, "pid": 1, "directory_mode": 0o700,
            "returncode": 0,
            "runtime_dir": "/fixture/query", "entries_before": [], "entries_after": [],
            "reranker": "none", "stderr": stderr,
            "stderr_sha256": hashlib.sha256(stderr.encode()).hexdigest()}


class RetrievalControlsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "a.py").write_text("alpha needle\n")
        (self.root / "b.py").write_text("beta needle\n")

    def record(self):
        rows = [("a.py", "alpha")]
        _, budget = controls.page(rows)
        arm = {**runner.score_case(["a.py"], rows, [5, 10]), **budget, "hits": ["a.py"]}
        arms = {name: copy.deepcopy(arm) for name in controls.ARMS}
        arms["codesage"]["runtime"] = fixture_runtime()
        for name in ("placebo", "rg_matched"):
            arms[name]["budget_bytes"] = arm["bytes"]
        return {"id": "q", "query": "needle", "expected_files": ["a.py"],
                "hits": ["a.py"], "first_hit_rank": 1, "arms": arms}

    def metadata(self) -> dict:
        meta = {"corpus": "fixture.yaml", "corpus_sha256": "0" * 64, "split": None,
                "salt": None, "limit": 10, "head": "not-a-git-repo", "model": "test/model",
                "reranker": "none", "device": "cpu", "control_seed": 0,
                "runner_sha256": "0" * 64, "controls_sha256": "0" * 64,
                "control_protocol": controls.CONTROL_PROTOCOL,
                "provenance": {**{k: "0" * 64 for k in (
                    "source_sha256", "eligible_files_sha256", "chunks_sha256", "semantic_files_sha256",
                    "artifact_digest", "config_sha256")},
                    "binary": self.fixture_pin("codesage"), "rg_binary": self.fixture_pin("rg"),
                    "embedding_artifacts": [{"label": name, **self.fixture_pin(name)}
                                            for name in ("tokenizer", "onnx", "ort_runtime")],
                    "reranker_artifacts": [], "eligible_files": ["a.py", "b.py"],
                    "source_manifest": [{"path": name, "sha256": "0" * 64} for name in ("a.py", "b.py")],
                    "environment": {}, "query_runtime": controls.QUERY_RUNTIME.copy()}}
        return self.synchronize_metadata(meta)

    def fixture_pin(self, name):
        return {"path": "/fixture/" + name, "size": 1, "sha256": "0" * 64}

    def synchronize_metadata(self, meta):
        provenance = meta["provenance"]
        provenance.update({key: meta[key] for key in ("head", "model", "reranker", "device")})
        provenance["artifact_digest"] = hashlib.sha256("".join(
            f"{pin['label']}={pin['sha256']}\n" for pin in provenance["embedding_artifacts"]).encode()).hexdigest()
        device = "cuda" if meta["device"].strip().lower() in ("gpu", "cuda") else meta["device"].strip().lower()
        runtime = "dylib" if any(pin["label"] == "ort_runtime" for pin in provenance["embedding_artifacts"]) else "static:0123456789abcdef"
        provenance["semantic_fingerprint"] = (f"v4;model={meta['model']};artifacts={provenance['artifact_digest']}"
                                               f";dim=768;pooling=mean;device={device};ort=api1.24/{runtime}"
                                               ";pipeline=1;maxseq=512;norm=l2;chunker=3;chunk=1500/350/200")
        provenance["source_sha256"] = controls.digest_json(provenance["source_manifest"])
        provenance["eligible_files_sha256"] = controls.digest_json(provenance["eligible_files"])
        return meta

    def test_byte_truncation_charges_duplicate_and_excluded_rows(self):
        rows = [("a.py", "alpha"), ("a.py", "again"), ("b.py", "beta")]
        emitted, charge = controls.page(rows, 18)
        self.assertEqual(charge["bytes"], 18)
        self.assertTrue(charge["truncated"])
        self.assertEqual(controls.distinct(emitted, "a.py"), [])
        self.assertEqual(controls.page(rows)[1]["bytes"], 32)

    def test_partial_path_is_not_a_hit(self):
        emitted, charge = controls.page([("a.py", "alpha")], 3)
        self.assertEqual(emitted, [])
        self.assertEqual(charge["bytes"], 3)

    def test_utf8_budget_counts_bytes(self):
        emitted, charge = controls.page([("a.py", "éé")], 8)
        self.assertEqual(charge["bytes"], 8)
        self.assertEqual(emitted[0][0], "a.py")

    def test_page_hash_matches_raw_byte_prefix_at_every_boundary(self):
        for rows in ([], [("a.py", "")],
                     [("é.py", "α🙂"), ("é.py", "again\n"), ("b.py", "")]):
            serialized = b"".join((p + "\n" + c + "\n").encode() for p, c in rows)
            for budget in (None, *range(len(serialized) + 2)):
                with self.subTest(rows=rows, budget=budget):
                    emitted, charge = controls.page(rows, budget)
                    prefix = serialized if budget is None else serialized[:budget]
                    self.assertEqual(charge, {
                        "bytes": len(prefix), "budget_bytes": budget,
                        "truncated": len(prefix) < len(serialized),
                        "page_sha256": hashlib.sha256(prefix).hexdigest(),
                    })
                    expected = []
                    offset = 0
                    for path, content in rows:
                        header = (path + "\n").encode()
                        body = (content + "\n").encode()
                        offset += len(header)
                        if offset > len(prefix):
                            break
                        expected.append((path, prefix[offset:offset + len(body)].decode(
                            "utf-8", errors="replace")))
                        offset += len(body)
                    self.assertEqual(emitted, expected)

    def test_page_does_not_visit_rows_after_truncation(self):
        class GuardedRows(list):
            def __iter__(self):
                yield self[0]
                raise AssertionError("row outside the byte budget was visited")

        rows = GuardedRows([("a.py", "alpha"), ("b.py", "unused")])
        emitted, charge = controls.page(rows, 8)
        self.assertEqual(emitted, [("a.py", "alp")])
        self.assertEqual(charge["page_sha256"], hashlib.sha256(b"a.py\nalp").hexdigest())

    def test_partial_path_does_not_encode_content(self):
        class UnusedContent(str):
            def __add__(self, other):
                raise AssertionError("content after a partial path was encoded")

        emitted, charge = controls.page([("é.py", UnusedContent("unused"))], 1)
        self.assertEqual(emitted, [])
        self.assertEqual(charge["page_sha256"], hashlib.sha256(b"\xc3").hexdigest())

    def test_seeded_uniform_file_order_uses_actual_chunks(self):
        chunks = {f"{i}.py": [f"chunk-{i}-a", f"chunk-{i}-b"] for i in range(20)}
        first = controls.placebo_rows(chunks, "case-a", 0)
        self.assertEqual(first, controls.placebo_rows(chunks, "case-a", 0))
        self.assertNotEqual(first, controls.placebo_rows(chunks, "case-b", 0))
        self.assertEqual({p for p, _ in first}, set(chunks))
        self.assertTrue(all(c in chunks[p] for p, c in first))

    def test_small_placebo_universe_still_matches_page_budget(self):
        rows = controls.placebo_rows({"a.py": ["alpha"]}, "q", 0, 100)
        emitted, cost = controls.page(rows, 100)
        self.assertEqual(cost["bytes"], 100)
        self.assertEqual([p for p, _ in controls.distinct(emitted)], ["a.py"])

    def test_rg_runs_real_sorted_literal_search_over_eligible_files(self):
        (self.root / "hidden.py").write_text("needle\n")
        rows, evidence = controls.run_rg(self.root, ["b.py", "a.py"], "needle", "rg")
        self.assertEqual([p for p, _ in rows], ["a.py", "b.py"])
        self.assertIn("--sort", evidence["command"])
        self.assertEqual(evidence["returncode"], 0)
        emitted, charge = controls.page(rows, 12)
        self.assertEqual(charge["bytes"], 12)
        self.assertEqual([p for p, _ in emitted], ["a.py"])

    def test_rg_empty_result_is_valid_but_invalid_query_is_not(self):
        self.assertEqual(controls.run_rg(self.root, ["a.py"], "absent", "rg")[0], [])
        with self.assertRaises(controls.InvalidRun):
            controls.run_rg(self.root, ["a.py"], "!!!", "rg")

    def test_rg_25_byte_page_ignores_ambient_max_count_configuration(self):
        (self.root / "a.py").write_text("needle\nneedle\n")
        (self.root / "b.py").write_text("needle\n")
        config = self.root / "rg-config"
        config.write_text("--max-count=1\n")
        empty_config = self.root / "empty-rg-config"
        empty_config.write_text("")
        rg_binary = real_rg()
        with patch.dict(os.environ, {"RIPGREP_CONFIG_PATH": str(empty_config)}):
            baseline, _ = controls.run_rg(self.root, ["a.py", "b.py"], "needle", rg_binary)
        with patch.dict(os.environ, {"RIPGREP_CONFIG_PATH": str(config)}):
            rows, evidence = controls.run_rg(self.root, ["a.py", "b.py"], "needle", rg_binary)
        self.assertIn("--no-config", evidence["command"])
        self.assertEqual(rows, baseline)
        emitted, cost = controls.page(rows, 25)
        self.assertEqual(cost["bytes"], 25)
        ranked = controls.distinct(emitted)
        self.assertEqual([path for path, _ in ranked], ["a.py"])
        self.assertEqual(runner.score_case(["b.py"], ranked, [5, 10])["recall@10"], 0.0)

    def test_rg_failure_is_not_zero_recall(self):
        with self.assertRaisesRegex(controls.InvalidRun, "rg failed"):
            controls.run_rg(self.root, ["missing.py"], "needle", "rg")

    def test_rg_large_universe_executes_batches_in_global_path_order(self):
        paths = [f"source-{i:03}.py" for i in range(300)]
        for path in paths:
            (self.root / path).write_text("needle\n")
        rows, evidence = controls.run_rg(self.root, list(reversed(paths)), "needle", "rg")
        self.assertEqual([p for p, _ in rows], paths)
        self.assertEqual(len(evidence["commands"]), 2)

    def test_invalid_search_is_not_a_valid_empty_page(self):
        for raw in ("", "no JSON", "{}", '{"results":null}', '[{"file_path":"a.py"}]'):
            with self.subTest(raw=raw), self.assertRaises(controls.InvalidRun):
                controls.search_rows(raw)
        self.assertEqual(controls.search_rows("[]"), [])

    def test_config_provenance_parses_toml_inline_comments(self):
        (self.root / ".codesage").mkdir()
        (self.root / ".codesage/config.toml").write_text('[embedding]\nmodel = "test/model" # selected\ndevice = "gpu"\n')
        self.assertEqual(runner.codesage_config(self.root)["embedding"]["model"], "test/model")

    def test_all_control_arms_and_exact_placebo_budget_are_required(self):
        record = self.record()
        controls.validate_record(record)
        for name in controls.ARMS:
            bad = copy.deepcopy(record)
            del bad["arms"][name]
            with self.subTest(name=name), self.assertRaisesRegex(controls.InvalidRun, "control arm"):
                controls.validate_record(bad)
        for bad_bytes in (record["arms"]["codesage"]["bytes"] - 1, 999):
            bad = copy.deepcopy(record)
            bad["arms"]["placebo"]["bytes"] = bad_bytes
            with self.assertRaisesRegex(controls.InvalidRun, "budget"):
                controls.validate_record(bad)

    def test_failed_control_and_disagreeing_real_hits_are_refused(self):
        bad = self.record()
        bad["arms"]["rg"]["error"] = "timeout"
        with self.assertRaisesRegex(controls.InvalidRun, "failed rg"):
            controls.validate_record(bad)
        bad = self.record()
        bad["hits"] = []
        with self.assertRaisesRegex(controls.InvalidRun, "disagree"):
            controls.validate_record(bad)

    def test_invented_control_score_is_refused(self):
        record = self.record()
        record["arms"]["rg"]["recall@10"] = 0.0
        with self.assertRaisesRegex(controls.InvalidRun, "score|disagrees"):
            controls.validate_record(record)

    def test_every_retained_file_metric_is_recomputed(self):
        for name in controls.ARMS:
            for metric in ("ndcg@10", "mrr", "noise_before_first_hit", "returned_count", "recall@5"):
                with self.subTest(arm=name, metric=metric):
                    bad = self.record()
                    bad["arms"][name][metric] += 1
                    with self.assertRaisesRegex(controls.InvalidRun, "disagrees"):
                        controls.validate_record(bad)
        bad = self.record()
        bad["arms"]["placebo"]["recall@1"] = 0.0
        with self.assertRaisesRegex(controls.InvalidRun, "recall@1"):
            controls.validate_record(bad)

    def test_search_forces_fresh_child_only_private_runtime(self):
        directory = self.root / ".codesage"
        directory.mkdir()
        (directory / "config.toml").write_text('[embedding]\nreranker="test/reranker"\n')
        binary = self.root / "query-fixture"
        binary.write_text(
            '#!/usr/bin/env python3\nimport json, os, pathlib, sys\n'
            'path = pathlib.Path(os.environ["CODESAGE_DAEMON_RUNTIME_DIR"])\n'
            'assert path.is_dir() and not list(path.iterdir())\n'
            'print(json.dumps({"runtime_dir":str(path)}))\n'
            'print("embedding model loaded\\nreranking privately\\nreranker loaded", file=sys.stderr)\n'
            'if sys.argv[-1] == "daemon": print("embedding through the running daemon", file=sys.stderr)\n'
            'if sys.argv[-1] == "socket": (path / "unexpected.sock").touch()\n'
        )
        binary.chmod(0o755)
        observed = []
        with patch.dict(os.environ, {"CODESAGE_DAEMON_RUNTIME_DIR": "/fixture/resident"}):
            for query in ("one", "two", "daemon", "socket"):
                with contextlib.redirect_stderr(io.StringIO()):
                    raw, error, runtime = runner.run_codesage_search(str(binary), self.root, query, 10)
                self.assertEqual(os.environ["CODESAGE_DAEMON_RUNTIME_DIR"], "/fixture/resident")
                self.assertEqual(json.loads(raw)["runtime_dir"], runtime["runtime_dir"])
                self.assertFalse(Path(runtime["runtime_dir"]).exists())
                observed.append(runtime["runtime_dir"])
                if query in ("one", "two"):
                    self.assertIsNone(error)
                    controls.validate_runtime(runtime)
                else:
                    self.assertEqual(error, "invalid-runtime")
        self.assertEqual(len(set(observed)), 4)

    def test_runtime_evidence_cannot_omit_private_reranker_or_rewrite_stderr(self):
        for change in ({"reranker": "configured"}, {"stderr_sha256": "changed"},
                       {"daemon_reuse": True}, {"entries_before": ["resident.sock"]},
                       {"entries_after": ["resident.sock"]}):
            with self.subTest(change=change), self.assertRaises(controls.InvalidRun):
                controls.validate_runtime({**fixture_runtime(), **change})

    def test_main_keeps_changed_source_run_incomplete(self):
        binary = self.fixture_index()
        corpus = self.root / "corpus.yaml"
        corpus.write_text(f"project_root: {self.root}\ncases:\n- id: q\n  query: needle\n  expected_files: [a.py]\n")
        output = self.root / "results.json"
        def search(*args):
            (self.root / "a.py").write_text("mutated source")
            return '[{"file_path":"a.py","content":"alpha"}]', None, fixture_runtime()
        with patch.object(runner, "run_codesage_search", side_effect=search), patch.object(
            sys, "argv", ["runner", str(corpus), "--codesage-bin", str(binary), "--results-json", str(output)]
        ), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(runner.main(), 3)
        saved = json.loads(output.read_text())
        self.assertFalse(saved["complete"])
        self.assertEqual(len(saved["records"]), 1)

    def test_main_failed_rg_arm_cannot_publish_success(self):
        binary = self.fixture_index()
        corpus = self.root / "corpus.yaml"
        corpus.write_text(f"project_root: {self.root}\ncases:\n- id: q\n  query: needle\n  expected_files: [a.py]\n")
        output = self.root / "results.json"
        with patch.object(runner, "run_codesage_search", return_value=(
            '[{"file_path":"a.py","content":"alpha"}]', None, fixture_runtime()
        )), patch.object(runner, "run_rg", side_effect=controls.InvalidRun("rg failed")), patch.object(
            sys, "argv", ["runner", str(corpus), "--codesage-bin", str(binary), "--results-json", str(output)]
        ), contextlib.redirect_stdout(io.StringIO()) as stdout, contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(runner.main(), 3)
        saved = json.loads(output.read_text())
        self.assertIn("invalid-control", saved["records"][0]["error"])
        self.assertIn("Control comparison invalid", stdout.getvalue())

    def fixture_index(self):
        directory = self.root / ".codesage"
        directory.mkdir()
        (directory / "config.toml").write_text('[embedding]\nmodel="test/model"\ndevice="cpu"\n')
        artifacts = []
        for label in ("tokenizer", "onnx"):
            path = self.root / label
            path.write_bytes(label.encode())
            artifacts.append((label, path, controls.file_pin(path)))
        digest = hashlib.sha256("".join(f"{label}={pin['sha256']}\n" for label, _, pin in artifacts).encode()).hexdigest()
        key = ";".join(f"{label}={path}:{pin['size']}:{path.stat().st_mtime_ns // 10**9}.{path.stat().st_mtime_ns % 10**9:09}"
                       for label, path, pin in artifacts)
        conn = sqlite3.connect(directory / "index.db")
        conn.executescript("""
            CREATE TABLE semantic_models(chunk_table, model, fingerprint, artifact_digest, artifact_stat_key);
            CREATE TABLE semantic_files(chunk_table, path, content_hash);
            CREATE TABLE chunks_test_fts(file_path, content, start_line, end_line);
        """)
        conn.execute("INSERT INTO semantic_models VALUES ('chunks_test','test/model','v4;test',?,?)", (digest, key))
        for path in ("a.py", "b.py"):
            text = (self.root / path).read_text()
            conn.execute("INSERT INTO semantic_files VALUES ('chunks_test',?,?)", (path, hashlib.sha256(text.encode()).hexdigest()))
            conn.execute("INSERT INTO chunks_test_fts VALUES (?,?,1,1)", (path, text))
        conn.commit()
        conn.close()
        binary = self.root / "codesage-fixture"
        binary.write_text('#!/usr/bin/env python3\nimport json\nprint(json.dumps({"semantic":{"state":"fresh"},"interpretation":{"stale_files":0}}))\n')
        binary.chmod(0o755)
        return binary

    def test_context_pins_actual_source_chunks_artifacts_and_rejects_stale_source(self):
        binary = self.fixture_index()
        context, chunks = controls.capture_context(self.root, str(binary), "rg", [])
        self.assertEqual(context["eligible_files"], ["a.py", "b.py"])
        self.assertEqual(chunks["a.py"], ["alpha needle\n"])
        self.assertEqual(context["semantic_fingerprint"], "v4;test")
        self.assertEqual(len(context["embedding_artifacts"]), 2)
        (self.root / "a.py").write_text("mutated")
        with self.assertRaisesRegex(controls.InvalidRun, "source differs"):
            controls.capture_context(self.root, str(binary), "rg", [])

    def test_artifact_same_size_rewrite_cannot_reuse_attestation(self):
        binary = self.fixture_index()
        path = self.root / "onnx"
        original = path.stat()
        path.write_bytes(b"xxxx")
        import os
        os.utime(path, ns=(original.st_atime_ns, original.st_mtime_ns))
        with self.assertRaisesRegex(controls.InvalidRun, "artifact bytes"):
            controls.capture_context(self.root, str(binary), "rg", [])

    def test_comparison_detects_model_device_and_index_identity_changes(self):
        base: dict = {"model": "a", "reranker": "a", "device": "cpu", "control_protocol": controls.CONTROL_PROTOCOL,
                "provenance": {k: "a" for k in compare.CONTROL_PROVENANCE_KEYS}}
        for key in ("model", "reranker", "device", "runner_sha256", "controls_sha256"):
            cand = copy.deepcopy(base)
            cand[key] = "b"
            self.assertTrue(any(x.startswith(key + ":") for x in compare.provenance_mismatch(base, cand)))
        for key in compare.CONTROL_PROVENANCE_KEYS:
            cand = copy.deepcopy(base)
            cand["provenance"][key] = "b"
            self.assertTrue(any(x.startswith("provenance." + key) for x in compare.provenance_mismatch(base, cand)))

    def test_comparison_requires_settings_and_both_instrument_hashes(self):
        meta: dict = self.metadata()
        for key in ("runner_sha256", "controls_sha256", "environment", "query_runtime"):
            bad = copy.deepcopy(meta)
            del (bad if key.endswith("sha256") else bad["provenance"])[key]
            path = self.root / "missing.json"
            path.write_text(json.dumps({"complete": True, "meta": bad, "records": [self.record()]}))
            with self.subTest(key=key), self.assertRaisesRegex(compare.Refused, "missing controlled-run"):
                compare.load_records([path], allow_partial=False)
        base = copy.deepcopy(meta)
        base["provenance"]["environment"] = {"CODESAGE_QUALIFIED_NAME_BOOST": "1"}
        cand = copy.deepcopy(base)
        cand["provenance"]["environment"]["CODESAGE_HYBRID"] = "never"
        self.assertTrue(any("environment" in m for m in compare.provenance_mismatch(base, cand)))

    def test_canonical_settings_exclude_scratch_paths_preserve_ranking_and_model_overrides(self):
        with patch.dict(os.environ, {"CODESAGE_DAEMON_RUNTIME_DIR": "/scratch/a",
                        "CODESAGE_BENCH_CORPUS_DIR": "/scratch/corpus",
                        "CODESAGE_QUALIFIED_NAME_BOOST": "1", "HF_HOME": "/models"}):
            settings = controls.instrument_environment()
            self.assertNotIn("CODESAGE_DAEMON_RUNTIME_DIR", settings)
            self.assertNotIn("CODESAGE_BENCH_CORPUS_DIR", settings)
            self.assertEqual(settings["CODESAGE_QUALIFIED_NAME_BOOST"], "1")
            self.assertEqual(settings["HF_HOME"], "/models")

    def test_matching_mixed_instruments_still_require_explicit_override(self):
        meta = {"control_protocol": controls.CONTROL_PROTOCOL,
                "controls_sha256": compare.MultiValue(["a", "b"]),
                "provenance": compare.MultiValue([
                    {"environment": {"CODESAGE_HYBRID": "never"}},
                    {"environment": {"CODESAGE_HYBRID": "always"}},
                ])}
        mismatches = compare.provenance_mismatch(meta, copy.deepcopy(meta))
        self.assertTrue(any("mixes instrument identities" in m for m in mismatches))
        self.assertTrue(any("mixes instrument settings" in m for m in mismatches))

    def test_invalid_instrument_identity_and_older_protocol_are_refused(self):
        for key, value in (("runner_sha256", None), ("controls_sha256", "unknown"),
                           ("control_protocol", "retrieval-controls-v1"),
                           ("environment", None), ("query_runtime", {})):
            bad: dict = self.metadata()
            (bad["provenance"] if key in ("environment", "query_runtime") else bad)[key] = value
            path = self.root / "invalid.json"
            path.write_text(json.dumps({"complete": True, "meta": bad, "records": [self.record()]}))
            with self.subTest(key=key), self.assertRaises(compare.Refused):
                compare.load_records([path], allow_partial=False)

    def test_comparison_refuses_omitted_arm_before_scoring(self):
        record = self.record()
        del record["arms"]["rg"]
        path = self.root / "result.json"
        path.write_text(json.dumps({"complete": True, "meta": self.metadata(), "records": [record]}))
        with self.assertRaisesRegex(compare.Refused, "control arm"):
            compare.load_records([path], allow_partial=False)

    def test_comparison_refuses_legacy_controlled_concatenation_in_both_orders(self):
        modern = self.root / "modern.json"
        modern.write_text(json.dumps({"complete": True, "meta": self.metadata(), "records": [self.record()]}))
        legacy_record = self.record()
        legacy_record["id"] = "legacy"
        del legacy_record["arms"]
        legacy = self.root / "legacy.json"
        legacy.write_text(json.dumps([legacy_record]))
        for paths in ([modern, legacy], [legacy, modern]):
            with self.subTest(paths=paths), self.assertRaisesRegex(compare.Refused, "controlled and legacy"):
                compare.load_records(paths, allow_partial=False)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as stderr:
                rc = compare.main(["--baseline", *map(str, paths), "--candidate", *map(str, paths),
                                   "--min-n", "1", "--bootstrap", "200", "--allow-mismatch"])
            self.assertEqual(rc, 2)
            self.assertIn("controlled and legacy", stderr.getvalue())
        meta, records = compare.load_records([legacy], allow_partial=False)
        self.assertEqual(meta, {})
        self.assertEqual(set(records), {"legacy"})
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as stderr:
            rc = compare.main(["--baseline", str(modern), "--candidate", str(legacy),
                               "--min-n", "1", "--bootstrap", "200", "--allow-mismatch"])
        self.assertEqual(rc, 2)
        self.assertIn("controlled inputs for both arms", stderr.getvalue())

    def test_cli_discloses_mismatch_override_and_hidden_ranking_setting(self):
        records = []
        for i in range(5):
            record = self.record()
            record["id"] = str(i)
            records.append(record)
        baseline = self.root / "baseline.json"
        baseline.write_text(json.dumps({"complete": True, "meta": self.metadata(), "records": records}))
        metadata = self.metadata()
        metadata["provenance"]["environment"]["CODESAGE_HYBRID"] = "never"
        candidate = self.root / "candidate.json"
        candidate.write_text(json.dumps({"complete": True, "meta": metadata, "records": records}))
        args = ["--baseline", str(baseline), "--candidate", str(candidate), "--min-n", "5", "--bootstrap", "200"]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(compare.main(args), 2)
        with contextlib.redirect_stdout(io.StringIO()) as stdout, contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(compare.main([*args, "--allow-mismatch"]), 0)
        self.assertIn("Provenance gate overridden", stdout.getvalue())
        self.assertIn("--allow-mismatch", stdout.getvalue())
        self.assertIn("provenance.environment", stdout.getvalue())
        self.assertIn("CODESAGE_HYBRID", stdout.getvalue())
        self.assertIn("never", stdout.getvalue())

    def controlled_input(self, label, ids, metadata):
        records: list[dict] = [{**self.record(), "id": str(cid)} for cid in ids]
        if metadata["reranker"] != "none":
            for record in records:
                runtime = record["arms"]["codesage"]["runtime"]
                runtime["reranker"] = metadata["reranker"]
                runtime["stderr"] += "reranking privately\nreranker loaded\n"
                runtime["stderr_sha256"] = hashlib.sha256(runtime["stderr"].encode()).hexdigest()
        path = self.root / f"{label}.json"
        path.write_text(json.dumps({"complete": True, "meta": metadata, "records": records}))
        return path

    def compare_cli(self, baseline, candidate, *extra):
        return subprocess.run([sys.executable, compare.__file__, "--baseline", *map(str, baseline),
                               "--candidate", *map(str, candidate), "--min-n", "6",
                               "--bootstrap", "200", *extra], capture_output=True, text=True, check=False)

    def test_split_input_cli_refuses_swapped_source_pins_in_every_file_order(self):
        first, second = self.metadata(), self.metadata()
        second["provenance"]["source_manifest"][0]["sha256"] = "1" * 64
        self.synchronize_metadata(second)
        baseline = [self.controlled_input("base-a", range(3), first),
                    self.controlled_input("base-b", range(3, 6), second)]
        candidate = [self.controlled_input("cand-a", range(3), second),
                     self.controlled_input("cand-b", range(3, 6), first)]
        for base in (baseline, baseline[::-1]):
            for cand in (candidate, candidate[::-1]):
                with self.subTest(base=base, cand=cand):
                    result = self.compare_cli(base, cand)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    for cid in range(6):
                        self.assertIn(f"case '{cid}': provenance.source_sha256", result.stderr)
                    self.assertIn("--allow-mismatch", result.stderr)
        result = self.compare_cli(baseline, candidate, "--allow-mismatch")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Provenance gate overridden with `--allow-mismatch`", result.stdout)
        for cid in range(6):
            self.assertIn(f"Allowed mismatch: case '{cid}': provenance.source_sha256", result.stdout)
        self.assertIn(first["provenance"]["source_sha256"], result.stdout)
        self.assertIn(second["provenance"]["source_sha256"], result.stdout)
        self.assertIn(str(baseline[0]), result.stdout)
        self.assertIn(str(candidate[0]), result.stdout)

    def test_loader_keeps_each_case_associated_with_all_origin_pins(self):
        keys = [*compare.PROVENANCE_KEYS, *compare.CONTROL_PROVENANCE_KEYS]
        keys.remove("control_protocol")
        keys.remove("query_runtime")
        keys.remove("environment")
        for key in keys:
            with self.subTest(key=key):
                first, second = self.metadata(), self.metadata()
                target = second if key in compare.PROVENANCE_KEYS else second["provenance"]
                if key in ("source_sha256", "eligible_files_sha256"):
                    second["provenance"]["source_manifest"][0]["sha256"] = "1" * 64
                    if key == "eligible_files_sha256":
                        second["provenance"]["eligible_files"].append("c.py")
                        second["provenance"]["source_manifest"].append({"path": "c.py", "sha256": "1" * 64})
                elif key in ("artifact_digest", "embedding_artifacts"):
                    second["provenance"]["embedding_artifacts"][0]["sha256"] = "1" * 64
                elif key in ("reranker_artifacts", "rg_binary"):
                    target[key] = [self.fixture_pin("reranker-extra")] if key == "reranker_artifacts" else self.fixture_pin("other-rg")
                elif key in ("split", "salt"):
                    second.update(split="heldout", salt="other-salt")
                elif key in ("limit", "control_seed"):
                    target[key] += 1
                else:
                    target[key] = ("1" * 40 if key == "head" else "1" * 64 if key.endswith("sha256")
                                   else "other/model" if key == "model" else "other/reranker" if key == "reranker"
                                   else "gpu" if key == "device" else "other-corpus")
                if key == "reranker":
                    second["provenance"]["reranker_artifacts"] = [self.fixture_pin("rerank-tokenizer"), self.fixture_pin("rerank-graph")]
                self.synchronize_metadata(second)
                if key == "semantic_fingerprint":
                    target[key] = target[key].replace("maxseq=512", "maxseq=256")
                baseline = [self.controlled_input("base-a", range(3), first),
                            self.controlled_input("base-b", range(3, 6), second)]
                candidate = [self.controlled_input("cand-a", range(3), second),
                             self.controlled_input("cand-b", range(3, 6), first)]
                bm, br = compare.load_records(baseline, allow_partial=False)
                cm, cr = compare.load_records(candidate, allow_partial=False)
                self.assertEqual(br["0"]["_input_provenance"]["provenance"], first["provenance"])
                differences = compare.paired_provenance_mismatch(bm, cm, br, cr)
                self.assertTrue(any(f"case '0': {'provenance.' if key in compare.CONTROL_PROVENANCE_KEYS else ''}{key}:"
                                    in difference for difference in differences), differences)

    def test_compatible_multi_project_cli_accepts_file_permutations_and_repartitioning(self):
        first, second = self.metadata(), self.metadata()
        second.update(corpus="other-corpus", corpus_sha256="1" * 64, head="1" * 40, model="other/model")
        second["provenance"]["source_manifest"][0]["sha256"] = "1" * 64
        second["provenance"]["embedding_artifacts"][0]["sha256"] = "1" * 64
        self.synchronize_metadata(second)
        baseline = [self.controlled_input("base-a", range(3), first),
                    self.controlled_input("base-b", range(3, 6), second)]
        candidate = [self.controlled_input("cand-a1", [0], first),
                     self.controlled_input("cand-a2", [1, 2], first),
                     self.controlled_input("cand-b", range(3, 6), second)]
        for base in (baseline, baseline[::-1]):
            for cand in (candidate, candidate[::-1]):
                with self.subTest(base=base, cand=cand):
                    result = self.compare_cli(base, cand)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("REJECT", result.stdout)
                    self.assertNotIn("overridden", result.stdout)

    def executable_fixture(self, codesage_name, rg_name):
        decoy = self.fixture_index()
        caller = self.root / "caller"
        caller.mkdir()
        log = self.root / "executed.jsonl"
        binary = caller / codesage_name
        binary.write_text(
            '#!/usr/bin/python3\nimport json, pathlib, sys\n'
            f'with open({str(log)!r}, "a") as log:\n'
            '    log.write(json.dumps({"path":__file__,"argv":sys.argv[1:]})+"\\n")\n'
            'if sys.argv[1] == "--version": print("codesage caller-fixture (release)")\n'
            'elif sys.argv[1] == "status": print(json.dumps({"semantic":{"state":"fresh"},"interpretation":{"stale_files":0}}))\n'
            'else:\n'
            '    print(json.dumps([{"file_path":"a.py","content":"alpha needle"}]))\n'
            '    print("embedding model loaded",file=sys.stderr)\n'
        )
        binary.chmod(0o755)
        rg = caller / rg_name
        # Resolve before the identity tests replace PATH with their own rg wrapper.
        rg.write_text(f'#!/bin/sh\nexec {shlex.quote(real_rg())} "$@"\n')
        rg.chmod(0o755)
        for name in {codesage_name, rg_name, decoy.name}:
            project_binary = self.root / name
            project_binary.write_text('#!/bin/sh\nexit 99\n')
            project_binary.chmod(0o755)
        corpus = self.root / "corpus.yaml"
        corpus.write_text(f"project_root: {self.root}\ncases:\n- id: q\n  query: needle\n  expected_files: [a.py]\n")
        return caller, binary, rg, log, corpus

    def exercise_executable_identity(self, selection):
        names = ("codesage", "rg") if selection == "path" else ("codesage-fixture", "rg-fixture")
        caller, binary, rg, log, corpus = self.executable_fixture(*names)
        output = self.root / "results.json"
        options = [] if selection == "path" else ["--codesage-bin", str(binary) if selection == "absolute" else "./" + binary.name,
                                                  "--rg-bin", str(rg) if selection == "absolute" else "./" + rg.name]
        original_cwd = Path.cwd()
        try:
            os.chdir(caller)
            with patch.dict(os.environ, {"PATH": ".:" + os.environ["PATH"]}), patch.object(
                    sys, "argv", ["runner", str(corpus), "--results-json", str(output), *options]), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(runner.main(), 0, stderr.getvalue())
        finally:
            os.chdir(original_cwd)
        saved = json.loads(output.read_text())
        self.assertTrue(saved["complete"])
        self.assertEqual(saved["meta"]["provenance"]["binary"], controls.file_pin(binary))
        self.assertEqual(saved["meta"]["provenance"]["rg_binary"], controls.file_pin(rg))
        arms = saved["records"][0]["arms"]
        self.assertEqual(arms["codesage"]["runtime"]["command"][0], str(binary))
        self.assertEqual(arms["rg"]["evidence"]["command"][0], str(rg))
        executed = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual([row["argv"][0] for row in executed], ["--version", "status", "search", "status"])
        self.assertEqual({row["path"] for row in executed}, {str(binary)})

    def test_relative_executable_pins_and_all_children_use_caller_identity(self):
        self.exercise_executable_identity("relative")

    def test_default_executables_resolve_relative_path_entries_once(self):
        self.exercise_executable_identity("path")

    def test_rg_fixture_supports_nonstandard_path_with_shell_characters(self):
        bin_dir = self.root / "ripgrep's bin"
        bin_dir.mkdir()
        shutil.copy2(real_rg(), bin_dir / "rg")
        with patch.dict(os.environ, {"PATH": str(bin_dir) + os.pathsep + os.environ["PATH"]}):
            self.exercise_executable_identity("path")

    def test_missing_real_rg_is_an_error_not_a_skipped_check(self):
        with patch.dict(os.environ, {"PATH": str(self.root / "missing-bin")}):
            with self.assertRaisesRegex(RuntimeError, "require ripgrep .* on PATH"):
                real_rg()

    def test_absolute_executable_identity_remains_accepted(self):
        self.exercise_executable_identity("absolute")

    def invalid_metadata_input(self, metadata):
        path = self.root / "invalid-metadata.json"
        path.write_text(json.dumps({"complete": True, "meta": metadata,
                                    "records": [{**self.record(), "id": str(i)} for i in range(6)]}))
        with self.assertRaises(compare.Refused):
            compare.load_records([path], allow_partial=False)
        result = self.compare_cli([path], [path], "--allow-mismatch")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("REFUSED", result.stderr)
        self.assertNotIn("# CodeSage paired comparison", result.stdout)

    def test_required_metadata_and_context_categories_refuse_absent_null_empty_wrong_shapes(self):
        optional_null = {"split", "salt"}
        metadata_keys = ("corpus", "corpus_sha256", "split", "salt", "limit", "head", "model", "reranker",
                         "device", "control_protocol", "control_seed", "runner_sha256", "controls_sha256")
        context_keys = ("source_sha256", "eligible_files_sha256", "chunks_sha256", "semantic_files_sha256",
                        "artifact_digest", "config_sha256", "semantic_fingerprint", "embedding_artifacts",
                        "reranker_artifacts", "binary", "rg_binary", "environment", "query_runtime",
                        "source_manifest", "eligible_files", "head", "model", "reranker", "device")
        for section, keys in ((None, metadata_keys), ("provenance", context_keys)):
            for key in keys:
                invalid_values = ["absent", None, "", [], {}]
                if section is None and key in optional_null:
                    invalid_values.remove(None)
                if section == "provenance" and key in ("environment", "reranker_artifacts"):
                    invalid_values.remove({} if key == "environment" else [])
                for value in invalid_values:
                    with self.subTest(section=section, key=key, value=value):
                        metadata = self.metadata()
                        target = metadata[section] if section else metadata
                        if value == "absent":
                            del target[key]
                        else:
                            target[key] = value
                        self.invalid_metadata_input(metadata)

    def test_pin_members_and_semantic_fingerprint_require_valid_evidence(self):
        for category in ("binary", "rg_binary", "embedding_artifacts", "reranker_artifacts"):
            for field, value in (("path", None), ("path", "relative"), ("path", "/"),
                                 ("path", "/fixture/../other"), ("sha256", None),
                                 ("sha256", "not-a-digest"), ("size", True), ("size", 0), ("size", -1)):
                with self.subTest(category=category, field=field, value=value):
                    metadata = self.metadata()
                    provenance = metadata["provenance"]
                    if category == "reranker_artifacts":
                        provenance[category] = [self.fixture_pin("tokenizer"), self.fixture_pin("graph")]
                    pin = provenance[category][0] if category.endswith("artifacts") else provenance[category]
                    pin[field] = value
                    self.invalid_metadata_input(metadata)
        for value in ("unknown", "nonsense", "v4;", "v4;garbage", "v4;model=test/model",
                      "v4;model=test/model;model=other;dim=768;device=cpu"):
            metadata = self.metadata()
            metadata["provenance"]["semantic_fingerprint"] = value
            with self.subTest(fingerprint=value):
                self.invalid_metadata_input(metadata)
        for value in ([self.fixture_pin("tokenizer")],
                      [{"label": "onnx", **self.fixture_pin("graph")}],
                      [{"label": "onnx", **self.fixture_pin("graph")}] * 2):
            metadata = self.metadata()
            metadata["provenance"]["embedding_artifacts"] = value
            with self.subTest(embedding=value):
                self.invalid_metadata_input(metadata)

    def test_manifest_digest_relationships_and_numeric_pins_refuse_contradictions(self):
        for key in ("source_sha256", "eligible_files_sha256", "artifact_digest"):
            metadata = self.metadata()
            metadata["provenance"][key] = "1" * 64
            with self.subTest(digest=key):
                self.invalid_metadata_input(metadata)
        for key, value in (("source_manifest", [{"path": "a.py", "sha256": None}]),
                           ("source_manifest", [{"path": "../a.py", "sha256": "0" * 64}]),
                           ("source_manifest", [{"path": "a.py", "sha256": "0" * 64}] * 2),
                           ("eligible_files", ["a.py", "a.py"]), ("eligible_files", ["../a.py"]),
                           ("environment", {"CODESAGE_HYBRID": None})):
            metadata = self.metadata()
            metadata["provenance"][key] = value
            with self.subTest(key=key, value=value):
                self.invalid_metadata_input(metadata)
        for key, value in (("limit", True), ("limit", 0), ("limit", -1), ("limit", 10.0),
                           ("control_seed", True), ("control_seed", 0.5), ("head", "unknown"),
                           ("model", "unknown"), ("device", "unknown")):
            metadata = self.metadata()
            metadata[key] = value
            with self.subTest(key=key, value=value):
                self.invalid_metadata_input(metadata)

    def test_configured_reranker_needs_two_distinct_pins_and_matching_private_runtime(self):
        metadata = self.metadata()
        metadata["reranker"] = "test/reranker"
        self.synchronize_metadata(metadata)
        for artifacts in (None, [], [self.fixture_pin("one")], [self.fixture_pin("one")] * 2):
            invalid = copy.deepcopy(metadata)
            invalid["provenance"]["reranker_artifacts"] = artifacts
            with self.subTest(artifacts=artifacts):
                self.invalid_metadata_input(invalid)
        metadata["provenance"]["reranker_artifacts"] = [self.fixture_pin("tokenizer"), self.fixture_pin("graph")]
        path = self.controlled_input("configured", range(6), metadata)
        self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        self.invalid_metadata_input(metadata)

    def test_optional_empty_values_and_legacy_disclosure_remain_supported(self):
        metadata = self.metadata()
        metadata["provenance"]["environment"] = {"CODESAGE_HYBRID": ""}
        path = self.controlled_input("empty-options", range(6), metadata)
        self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        metadata.update(split="heldout", salt="test-1")
        path = self.controlled_input("split", range(6), metadata)
        self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        for split, salt in ((None, "extra"), ("train", None), ("train", ""), ("invalid", "s")):
            bad = self.metadata()
            bad.update(split=split, salt=salt)
            self.invalid_metadata_input(bad)
        legacy = self.root / "legacy-only.json"
        records = [{**self.record(), "id": str(i)} for i in range(6)]
        for record in records:
            del record["arms"]
        legacy.write_text(json.dumps(records))
        result = self.compare_cli([legacy], [legacy])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("uncontrolled", result.stdout)

    def test_actual_r4_controlled_capture_passes_strict_metadata_loader_and_cli(self):
        path = Path(__file__).parent / "public-corpus-results/requests-controls-2026-10-03/results.json"
        metadata, records = compare.load_records([path], allow_partial=False)
        self.assertEqual(len(records), 20)
        self.assertEqual(metadata["run_at"], "2026-10-03T16:52:04Z")
        result = self.compare_cli([path], [path])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("uncontrolled", result.stdout)
        self.assertNotIn("overridden", result.stdout)

    def actual_capture(self):
        path = Path(__file__).parent / "public-corpus-results/requests-controls-2026-10-03/results.json"
        return json.loads(path.read_text())

    def rewrite_fingerprint(self, envelope, fields, version="v4"):
        envelope["meta"]["provenance"]["semantic_fingerprint"] = version + ";" + ";".join(
            f"{name}={value}" for name, value in fields.items())

    def capture_fingerprint_fields(self, envelope):
        return dict(part.split("=", 1) for part in envelope["meta"]["provenance"]["semantic_fingerprint"].split(";")[1:])

    def assert_invalid_controlled_envelope(self, envelope, message):
        path = self.root / "invalid-capture.json"
        path.write_text(json.dumps(envelope))
        with self.assertRaisesRegex(compare.Refused, message):
            compare.load_records([path], allow_partial=False)
        for extra in ((), ("--allow-mismatch",)):
            result = self.compare_cli([path], [path], *extra)
            self.assertEqual(result.returncode, 2, result.stdout)
            self.assertIn(message, result.stderr)
            self.assertNotIn("# CodeSage paired comparison", result.stdout)

    def test_actual_capture_requires_every_v4_fingerprint_field(self):
        required = ("model", "artifacts", "dim", "pooling", "device", "ort", "pipeline",
                    "maxseq", "norm", "chunker", "chunk")
        for name in required:
            with self.subTest(omitted=name):
                envelope = self.actual_capture()
                fields = self.capture_fingerprint_fields(envelope)
                del fields[name]
                self.rewrite_fingerprint(envelope, fields)
                self.assert_invalid_controlled_envelope(envelope, "semantic_fingerprint")
        envelope = self.actual_capture()
        fields = self.capture_fingerprint_fields(envelope)
        self.rewrite_fingerprint(envelope, {k: fields[k] for k in ("model", "artifacts", "dim", "device")})
        self.assert_invalid_controlled_envelope(envelope, "complete v4 schema")

    def test_actual_capture_refuses_invalid_v4_fingerprint_domains_and_versions(self):
        mutations = {
            "model": ("other/model", ""), "artifacts": ("0" * 64, "not-a-digest"),
            "dim": ("0", "-1", "1.5", "18446744073709551616"),
            "pooling": ("banana", "Mean"), "device": ("cpu", "gpu", "unknown"),
            "ort": ("api1.24", "api1.24/unknown", "api2.24/dylib", "api1.24/static:short"),
            "pipeline": ("0", "-1", "true", "4294967296"),
            "maxseq": ("0", "-1", "none", "18446744073709551616"),
            "norm": ("invalid", "L2"), "chunker": ("0", "-1", "3.5", "4294967296"),
            "chunk": ("0/350/200", "1500/-1/200", "1500/350/-1", "1500/350", "1500/350/200/0"),
        }
        for name, values in mutations.items():
            for value in values:
                with self.subTest(field=name, value=value):
                    envelope = self.actual_capture()
                    fields = self.capture_fingerprint_fields(envelope)
                    fields[name] = value
                    self.rewrite_fingerprint(envelope, fields)
                    self.assert_invalid_controlled_envelope(envelope, "semantic_fingerprint")
        for version in ("v3", "v999", "V4"):
            with self.subTest(version=version):
                envelope = self.actual_capture()
                self.rewrite_fingerprint(envelope, self.capture_fingerprint_fields(envelope), version)
                self.assert_invalid_controlled_envelope(envelope, "semantic_fingerprint")
        for suffix in (";pipeline=1", ";unknown=1"):
            envelope = self.actual_capture()
            envelope["meta"]["provenance"]["semantic_fingerprint"] += suffix
            self.assert_invalid_controlled_envelope(envelope, "semantic_fingerprint")

    def test_actual_capture_dynamic_runtime_pin_cannot_be_removed_with_recomputed_digests(self):
        envelope = self.actual_capture()
        provenance = envelope["meta"]["provenance"]
        provenance["embedding_artifacts"] = [p for p in provenance["embedding_artifacts"] if p["label"] != "ort_runtime"]
        provenance["artifact_digest"] = hashlib.sha256("".join(
            f"{p['label']}={p['sha256']}\n" for p in provenance["embedding_artifacts"]).encode()).hexdigest()
        fields = self.capture_fingerprint_fields(envelope)
        fields["artifacts"] = provenance["artifact_digest"]
        self.rewrite_fingerprint(envelope, fields)
        self.assert_invalid_controlled_envelope(envelope, "ort/artifact labels")

    def test_actual_capture_runtime_model_provider_dim_and_pooling_must_agree_with_fingerprint(self):
        for field, value in (("model", "other/model"), ("device", "cpu"), ("dim", "384"), ("pooling", "cls")):
            with self.subTest(field=field):
                envelope = self.actual_capture()
                fields = self.capture_fingerprint_fields(envelope)
                fields[field] = value
                if field in ("model", "device"):
                    envelope["meta"][field] = value
                    envelope["meta"]["provenance"][field] = value
                self.rewrite_fingerprint(envelope, fields)
                self.assert_invalid_controlled_envelope(envelope, "disagrees with semantic fingerprint")
        envelope = self.actual_capture()
        runtime = envelope["records"][0]["arms"]["codesage"]["runtime"]
        runtime["stderr"] += "CODESAGE_ALLOW_CPU_FALLBACK: CUDA requested, CPU session\n"
        runtime["stderr_sha256"] = hashlib.sha256(runtime["stderr"].encode()).hexdigest()
        self.assert_invalid_controlled_envelope(envelope, "degraded CPU fallback")

    def test_valid_static_dynamic_sidecar_and_alternate_fingerprint_fields_remain_supported(self):
        for device in ("cpu", " CPU ", "GPU", " cuda ", "CoreML"):
            for sidecar in (False, True):
                with self.subTest(device=device, sidecar=sidecar):
                    metadata = self.metadata()
                    metadata["device"] = device
                    pins = metadata["provenance"]["embedding_artifacts"]
                    if device == "CoreML":
                        pins.pop()
                    if sidecar:
                        pins.insert(2, {"label": "onnx_data", **self.fixture_pin("weights")})
                    self.synchronize_metadata(metadata)
                    metadata["provenance"]["device"] = device.strip().lower()
                    path = self.controlled_input("valid-provider", range(6), metadata)
                    self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        metadata = self.metadata()
        metadata["provenance"]["embedding_artifacts"].pop()
        self.synchronize_metadata(metadata)
        fingerprint = metadata["provenance"]["semantic_fingerprint"]
        for old, new in (("dim=768", "dim=384"), ("pooling=mean", "pooling=cls"),
                         ("maxseq=512", "maxseq=256"), ("norm=l2", "norm=none"),
                         ("chunk=1500/350/200", "chunk=1024/0/0")):
            fingerprint = fingerprint.replace(old, new)
        metadata["provenance"]["semantic_fingerprint"] = fingerprint
        path = self.controlled_input("valid-static-alternates", range(6), metadata)
        self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        envelope = json.loads(path.read_text())
        envelope["meta"]["provenance"]["embedding_artifacts"].append(
            {"label": "ort_runtime", **self.fixture_pin("unexpected-dynamic-runtime")})
        provenance = envelope["meta"]["provenance"]
        provenance["artifact_digest"] = hashlib.sha256("".join(
            f"{p['label']}={p['sha256']}\n" for p in provenance["embedding_artifacts"]).encode()).hexdigest()
        fields = self.capture_fingerprint_fields(envelope)
        fields["artifacts"] = provenance["artifact_digest"]
        self.rewrite_fingerprint(envelope, fields)
        self.assert_invalid_controlled_envelope(envelope, "ort/artifact labels")

    def test_actual_capture_requires_declared_text_queries_even_with_override(self):
        for value in ("absent", None, {}, [], 42, True):
            with self.subTest(query=value):
                envelope = self.actual_capture()
                for record in envelope["records"]:
                    if value == "absent":
                        del record["query"]
                    else:
                        record["query"] = value
                self.assert_invalid_controlled_envelope(envelope, "non-text controlled query")

    def test_text_query_contract_preserves_empty_unicode_and_legacy_paths(self):
        for query in ("", "find café λ code"):
            path = self.controlled_input("text-query", range(6), self.metadata())
            envelope = json.loads(path.read_text())
            for record in envelope["records"]:
                record["query"] = query
            path.write_text(json.dumps(envelope))
            self.assertEqual(self.compare_cli([path], [path]).returncode, 0)
        candidate = self.root / "different-query.json"
        envelope["records"][0]["query"] = "different text"
        candidate.write_text(json.dumps(envelope))
        for extra in ((), ("--allow-mismatch",)):
            result = self.compare_cli([path], [candidate], *extra)
            self.assertEqual(result.returncode, 2)
            self.assertIn("query or gold differs", result.stderr)
        legacy = self.root / "legacy-no-query.json"
        records = [{"id": str(i), "hits": ["a.py"], "expected_files": ["a.py"]} for i in range(6)]
        legacy.write_text(json.dumps(records))
        result = self.compare_cli([legacy], [legacy])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("uncontrolled", result.stdout)

    def test_modern_completion_and_universe_evidence_cannot_contradict_records(self):
        path = self.controlled_input("completion", range(6), self.metadata())
        envelope = json.loads(path.read_text())
        for value in (None, "true", [], {}):
            path.write_text(json.dumps({**envelope, "complete": value}))
            result = self.compare_cli([path], [path], "--allow-partial", "--allow-mismatch")
            self.assertEqual(result.returncode, 2, result.stdout)
            self.assertIn("completion flag", result.stderr)
        path.write_text(json.dumps({**envelope, "complete": False}))
        self.assertEqual(self.compare_cli([path], [path]).returncode, 2)
        self.assertEqual(self.compare_cli([path], [path], "--allow-partial").returncode, 0)
        metadata = self.metadata()
        metadata["provenance"]["eligible_files"] = ["b.py"]
        metadata["provenance"]["source_manifest"] = [metadata["provenance"]["source_manifest"][1]]
        self.synchronize_metadata(metadata)
        path = self.controlled_input("missing-gold", range(6), metadata)
        result = self.compare_cli([path], [path], "--allow-mismatch")
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("outside pinned eligible universe", result.stderr)

    def test_symmetric_mixed_settings_cli_discloses_each_arm_case_input_and_value(self):
        first, second = self.metadata(), self.metadata()
        first["provenance"]["environment"] = {"CODESAGE_HYBRID": "always", "HF_HOME": ""}
        second["provenance"]["environment"] = {"CODESAGE_HYBRID": "never"}
        baseline = [self.controlled_input("baseline-always", range(3), first),
                    self.controlled_input("baseline-never", range(3, 6), second)]
        candidate = [self.controlled_input("candidate-always", range(3), first),
                     self.controlled_input("candidate-never", range(3, 6), second)]
        for base in (baseline, baseline[::-1]):
            for cand in (candidate, candidate[::-1]):
                result = self.compare_cli(base, cand)
                self.assertEqual(result.returncode, 2, result.stdout)
                result = self.compare_cli(base, cand, "--allow-mismatch")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("Provenance gate overridden", result.stdout)
                for arm, paths in (("baseline", baseline), ("candidate", candidate)):
                    for cid in range(6):
                        prefix = f"Allowed mismatch: {arm} mixes instrument settings: case '{cid}', input {str(paths[cid // 3])!r}, "
                        self.assertIn(prefix + f"provenance.environment.CODESAGE_HYBRID={'always' if cid < 3 else 'never'!r}", result.stdout)
                        self.assertIn(prefix + "provenance.environment.HF_HOME=" + ("''" if cid < 3 else "<unset>"), result.stdout)

    def test_git_walk_uses_one_pin_even_when_head_changes(self):
        seen = []
        import subprocess
        def git(_project, args, **kwargs):
            seen.append(args)
            if args[0] == "rev-parse":
                return subprocess.CompletedProcess(args, 0, "a" * 40 + "\n", "")
            if args[0] == "rev-list":
                return subprocess.CompletedProcess(args, 0, "a" * 40 + "\n", "")
            return subprocess.CompletedProcess(args, 0, b"", b"")
        with patch.object(self_eval, "_git", side_effect=git):
            self_eval.git_log_commits(self.root, 10)
        self.assertEqual(sum(c[0] == "rev-parse" for c in seen), 1)
        self.assertIn("a" * 40, seen[1])
        self.assertIn("a" * 40, seen[2])
        self.assertFalse(any("HEAD" in arg for cmd in seen[1:] for arg in cmd))

    def test_git_child_policy_preserves_hardening_and_parent_environment(self):
        import os
        selectors = {"GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR",
                     "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_IMPLICIT_WORK_TREE",
                     "GIT_GRAFT_FILE", "GIT_REPLACE_REF_BASE", "GIT_PREFIX", "GIT_SHALLOW_FILE", "GIT_NAMESPACE",
                     "GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_REFERENCE_BACKEND"}
        self.assertEqual(controls.GIT_REPOSITORY_SELECTORS, selectors)
        with patch.dict(os.environ, {**{key: "decoy" for key in selectors},
                                     "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "safe.directory",
                                     "GIT_CONFIG_VALUE_0": "fixture", "GIT_NO_REPLACE_OBJECTS": "1",
                                     "CONTROL_TEST_MARKER": "preserved"}):
            parent = dict(os.environ)
            child = controls.git_environment()
            self.assertFalse(selectors & child.keys())
            self.assertEqual(child["GIT_CONFIG_COUNT"], "1")
            self.assertEqual(child["GIT_CONFIG_KEY_0"], "safe.directory")
            self.assertEqual(child["GIT_CONFIG_VALUE_0"], "fixture")
            self.assertEqual(child["GIT_NO_REPLACE_OBJECTS"], "1")
            self.assertEqual(child["CONTROL_TEST_MARKER"], "preserved")
            self.assertEqual(dict(os.environ), parent)

    def test_self_eval_source_digest_changes_with_read_source(self):
        conn = sqlite3.connect(":memory:")
        conn.row_factory = sqlite3.Row
        self.addCleanup(conn.close)
        conn.executescript("CREATE TABLE files(path); INSERT INTO files VALUES ('a.py');")
        pinned = self_eval.source_snapshot_digest(self.root, conn)
        (self.root / "a.py").write_text("changed")
        self.assertNotEqual(pinned, self_eval.source_snapshot_digest(self.root, conn))


if __name__ == "__main__":
    unittest.main()
