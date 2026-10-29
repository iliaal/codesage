import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[2] / "bench/generate-llm-corpus.py"
spec = importlib.util.spec_from_file_location("generate_llm_corpus_output", SCRIPT)
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)


class CorpusOutputTests(unittest.TestCase):
    def run_generator(self, root, queries, output=None):
        sources = [root / f"source{i}.py" for i in range(len(queries))]
        argv = [str(SCRIPT), "--project-root", str(root)]
        if output is not None:
            argv.extend(["--out", str(output)])
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch("sys.argv", argv), \
                patch.object(gen, "candidate_files", return_value=sources), \
                patch.object(gen, "generate_query", side_effect=queries), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = gen.main()
        return status, stdout.getvalue(), stderr.getvalue()

    def test_all_failed_queries_leave_existing_output_untouched(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "corpus.yaml"
            original = b"previous corpus\n"
            output.write_bytes(original)
            status, stdout, stderr = self.run_generator(root, [None, None], output)
            self.assertEqual(status, 1)
            self.assertEqual(output.read_bytes(), original)
            self.assertEqual(stdout, "")
            self.assertIn("no usable queries generated", stderr)

    def test_all_failed_queries_create_no_output_or_parent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "new" / "corpus.yaml"
            status, _, _ = self.run_generator(root, [None], output)
            self.assertEqual(status, 1)
            self.assertFalse(output.parent.exists())

    def test_all_failed_queries_emit_no_stdout_corpus(self):
        with tempfile.TemporaryDirectory() as directory:
            status, stdout, _ = self.run_generator(Path(directory), [None])
            self.assertEqual(status, 1)
            self.assertEqual(stdout, "")

    def test_partial_success_still_writes_usable_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "corpus.yaml"
            status, stdout, _ = self.run_generator(root, [None, "find parsing logic"], output)
            self.assertEqual(status, 0)
            self.assertEqual(stdout, "")
            corpus = output.read_text()
            self.assertIn("query: 'find parsing logic'", corpus)
            self.assertEqual(corpus.count("  - id:"), 1)


if __name__ == "__main__":
    unittest.main()
