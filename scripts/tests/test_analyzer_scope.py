"""Scorecard scope disclosures must describe the actual mtime selection."""

import datetime
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).parents[2]
SCRIPTS = [ROOT / "bench" / f"analyze-codesage-{kind}.py"
           for kind in ("quality", "usage")]
SCOPE = "**Event scope**: entire selected transcripts; event timestamps are not filtered."


def write_transcript(path, timestamps):
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = []
    for index, timestamp in enumerate(timestamps):
        rows.extend([
            {"timestamp": timestamp, "message": {"role": "assistant", "content": [
                {"type": "tool_use", "name": "mcp__codesage__search",
                 "id": str(index), "input": {"query": "buffer"}}]}},
            {"timestamp": timestamp, "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": str(index),
                 "content": '{"results": [{"file": "src/buffer.rs"}]}'}]}},
        ])
    path.write_text("\n".join(json.dumps(row) for row in rows), encoding="utf-8")


class AnalyzerScopeTests(unittest.TestCase):
    def test_cli_discloses_full_history_for_selected_transcripts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            projects = root / "projects"
            recent = datetime.datetime.now(datetime.timezone.utc).isoformat()
            # An old call survives in a recently modified/resumed transcript.
            write_transcript(projects / "project" / "resumed.jsonl",
                             ["2000-01-01T00:00:00Z", recent, None, "invalid"])
            # A recent event in an old file is excluded by file mtime.
            old_file = projects / "project" / "old-file.jsonl"
            write_transcript(old_file, [recent])
            os.utime(old_file, (946684800, 946684800))
            for script in SCRIPTS:
                for days in (1, 7):
                    for output_file in (False, True):
                        with self.subTest(script=script.name, days=days,
                                          output_file=output_file):
                            command = [sys.executable, str(script), "--projects-root",
                                       str(projects), "--window-days", str(days)]
                            output = root / "scorecard.md"
                            if output_file:
                                command.extend(["--output", str(output)])
                            result = subprocess.run(command, capture_output=True,
                                                    text=True, check=True)
                            report = output.read_text() if output_file else result.stdout
                            self.assertIn(f"**Transcript selection**: files modified within "
                                          f"the last {days} days", report)
                            self.assertIn(SCOPE, report)
                            self.assertNotIn("**Window**: last", report)
                            self.assertIn("**Transcripts scanned**: 1 ", report)
                            count = ("**CodeSage tool_results analyzed**" if "quality" in
                                     script.name else "**Calls observed**")
                            self.assertIn(f"{count}: 4 ", report)

    def test_cli_help_explains_mtime_and_full_history(self):
        for script in SCRIPTS:
            with self.subTest(script=script.name):
                result = subprocess.run([sys.executable, str(script), "--help"],
                                        capture_output=True, text=True, check=True)
                help_text = " ".join(result.stdout.split())
                self.assertIn("file modification time", help_text)
                self.assertIn("full history, without filtering event timestamps", help_text)

    def test_selection_boundary_and_quality_subagents(self):
        for script in SCRIPTS:
            with self.subTest(script=script.name), tempfile.TemporaryDirectory() as directory:
                spec = importlib.util.spec_from_file_location(script.stem, script)
                module = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(module)
                root = Path(directory)
                cutoff = 1700000000
                for offset in (-1, 0, 1):
                    for parent in (root / "project", root / "project/session/subagents"):
                        path = parent / f"event-{offset}.jsonl"
                        write_transcript(path, ["2000-01-01T00:00:00Z"])
                        os.utime(path, (cutoff + offset, cutoff + offset))
                selected = module.iter_transcripts(root, cutoff)
                self.assertEqual(sorted(path.name for path in selected),
                                 sorted(["event-0.jsonl", "event-1.jsonl"] *
                                        (2 if "quality" in script.name else 1)))


if __name__ == "__main__":
    unittest.main()
