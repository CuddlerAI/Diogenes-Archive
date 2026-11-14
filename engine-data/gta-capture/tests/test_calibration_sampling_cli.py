"""Output collision checks must run before reading or overwriting sealed inputs."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import helpers  # noqa: F401


_SPEC = importlib.util.spec_from_file_location(
    "collect_projection_samples_cli",
    Path(__file__).resolve().parents[1] / "tools" / "collect-projection-samples.py")
collector_cli = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(collector_cli)


class CalibrationSamplingCliTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.inputs = [self.root / "lane-1.jsonl", self.root / "lane-2.jsonl"]
        for source in self.inputs:
            source.write_bytes(b"sealed frame-event input\n")

    def invoke(self, out, report):
        arguments = ["collect-projection-samples.py", "--frames",
                     *map(str, self.inputs), "--game-build", "test-fixture",
                     "--captured-at", "2026-09-08T00:00:00Z",
                     "--out", str(out), "--report", str(report)]
        stderr = io.StringIO()
        with (patch.object(sys, "argv", arguments),
              patch.object(collector_cli, "samples_from_native_frames",
                           return_value=({"samples": []}, {"fixture": True})) as collect,
              redirect_stdout(io.StringIO()), redirect_stderr(stderr)):
            result = collector_cli.main()
        return result, collect, stderr.getvalue()

    def test_same_output_file_is_rejected_without_overwriting(self):
        output = self.root / "existing.json"
        output.write_bytes(b"existing result")
        result, collect, error = self.invoke(output, self.root / "." / output.name)
        self.assertEqual(result, 1)
        collect.assert_not_called()
        self.assertIn("different files", error)
        self.assertEqual(output.read_bytes(), b"existing result")

    def test_either_output_cannot_replace_any_input(self):
        for source in self.inputs:
            for replace_report in (False, True):
                with self.subTest(source=source.name, replace_report=replace_report):
                    other = self.root / "unwritten.json"
                    result, collect, error = self.invoke(
                        other if replace_report else source,
                        source if replace_report else other)
                    self.assertEqual(result, 1)
                    collect.assert_not_called()
                    self.assertIn("frame-event inputs", error)
                    self.assertFalse(other.exists())
                    self.assertEqual(source.read_bytes(), b"sealed frame-event input\n")

    def test_output_hardlink_to_input_is_rejected(self):
        alias = self.root / "input-alias.json"
        try:
            os.link(self.inputs[0], alias)
        except OSError as exc:
            self.skipTest(f"filesystem does not support hardlinks: {exc}")
        result, collect, error = self.invoke(alias, self.root / "report.json")
        self.assertEqual(result, 1)
        collect.assert_not_called()
        self.assertIn("frame-event inputs", error)
        self.assertEqual(alias.read_bytes(), b"sealed frame-event input\n")

    def test_distinct_outputs_are_written_and_inputs_preserved(self):
        output, report = self.root / "samples.json", self.root / "report.json"
        result, collect, error = self.invoke(output, report)
        self.assertEqual(result, 0, error)
        collect.assert_called_once()
        self.assertEqual(json.loads(output.read_text(encoding="utf-8")), {"samples": []})
        self.assertEqual(json.loads(report.read_text(encoding="utf-8")), {"fixture": True})
        for source in self.inputs:
            self.assertEqual(source.read_bytes(), b"sealed frame-event input\n")


if __name__ == "__main__":
    unittest.main()
