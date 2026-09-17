#!/usr/bin/env python3
"""Ablation environment isolation regressions: python3 bench/test_ablation.py."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("ablation", Path(__file__).with_name("ablation.py"))
ablation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ablation)


class AblationEnvironmentTests(unittest.TestCase):
    def test_each_arm_ignores_ambient_tuning_and_retains_operational_environment(self):
        # Independently enumerate the supported tuning switches so omitting a key
        # from the sanitizer actually contaminates the runner and fails this test.
        tuning_keys = {
            "CODESAGE_RRF_K",
            "CODESAGE_BM25_WEIGHT",
            "CODESAGE_DEFINITION_BOOST",
            "CODESAGE_PATH_PENALTY",
            "CODESAGE_QUALIFIED_NAME_BOOST",
            "CODESAGE_STEM_SCAN",
            "CODESAGE_TEST_QUERY_AWARE",
            "CODESAGE_FILE_SATURATION",
            "CODESAGE_DIR_SATURATION",
            "CODESAGE_DIR_SATURATION_THRESHOLD",
            "CODESAGE_DIR_SATURATION_DECAY",
            "CODESAGE_ADAPTIVE_RERANK",
            "CODESAGE_VERSION_DEMOTE",
            "CODESAGE_PLATFORM_DEMOTE",
            "CODESAGE_PHP_DECLARATION_DEMOTE",
            "CODESAGE_FUSED_RERANK",
            "CODESAGE_STEM_MATCH_BOOST",
            "CODESAGE_HYBRID",
            "CODESAGE_MENTION_ANCHOR",
            "CODESAGE_QUALIFIED_GROUPS",
        }
        ambient_tuning = dict.fromkeys(tuning_keys, "ambient-only-sentinel")
        ambient_tuning["CODESAGE_STEM_SCAN"] = ""  # Presence, not truthiness, matters.
        operational = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": "/operational-home",
            "CODESAGE_WATCH": "0",
            "CODESAGE_DAEMON_RUNTIME_DIR": "/operational-runtime",
            "CODESAGE_NVIDIA_LIBS": "/operational-cuda",
            "CODESAGE_BENCH_CORPUS_DIR": "/operational-corpus",
            "ORT_DYLIB_PATH": "/operational-ort.so",
            "HF_HOME": "/operational-model-cache",
            "HF_TOKEN": "credential-must-not-be-printed",
        }
        ambient = {**operational, **ambient_tuning}
        selected = list(ablation.ARMS)
        stderr = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            capture = root / "environments.jsonl"
            runner = root / "capture-runner.py"
            runner.write_text(
                "import json, os, sys\n"
                "with open(sys.argv[1], 'a') as capture:\n"
                "    capture.write(json.dumps(dict(os.environ)) + '\\n')\n"
                "print('<!-- METRICS: miss_rate=0 median_first=1 r5=1 r10=1 '\n"
                "      'mean_tokens_to_hit=10 search_failures=0 -->')\n",
                encoding="utf-8",
            )
            with patch.dict(os.environ, ambient, clear=True):
                with contextlib.redirect_stderr(stderr):
                    _, _, _, invalid = ablation.run_sweep(
                        [capture], selected, runner, "codesage", 10
                    )
                self.assertEqual(dict(os.environ), ambient)
            environments = [json.loads(line) for line in capture.read_text().splitlines()]

        self.assertEqual(invalid, [])
        self.assertEqual(len(environments), len(selected))
        for arm, environment in zip(selected, environments):
            with self.subTest(arm=arm):
                self.assertEqual(
                    {key: environment[key] for key in tuning_keys if key in environment},
                    ablation.ARMS[arm][0],
                )
                self.assertEqual({key: environment.get(key) for key in operational}, operational)
        stripped_lines = [line for line in stderr.getvalue().splitlines() if "stripped" in line]
        for key in tuning_keys:
            self.assertEqual(sum(line.endswith(f": {key}") for line in stripped_lines), len(selected))
        self.assertNotIn("ambient-only-sentinel", stderr.getvalue())
        self.assertNotIn(operational["HF_TOKEN"], stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
