import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from diogenes_wm.eval import cp2077_camera_report as camera_report

from diogenes_wm.eval.cp2077_camera_report import (
    ARCHIVE_BYTES, BLOCK_SCHEMA, EXPANDED_REPORT_BYTES, V2, V3, decode_block, read_camera_report,
)


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


class CameraReportTests(unittest.TestCase):
    def fixture(self, root):
        # Keep numeric spellings literal: a float load/dump round-trip must not pass the SHA check.
        value = b'[-0.0,5e-324,-1.0000003576278687,18446744073709551615]'
        draw = b'{"matrix":' + value + b',"before":true}\n'
        block = (b'{"schema":"' + BLOCK_SCHEMA.encode() + b'","keys":["matrix","before"],'
                 b'"values":[' + value + b',true],"rows":[[[0,0],[1,1]],[[0,0],[1,1]]],'
                 b'"expanded_bytes":' + str(2 * len(draw)).encode() + b'}\n')
        path = root / "render-camera-observation.json"
        (root / "render-camera-draws.jsonl").write_bytes(block)
        report = {"schema": V3, "non_trainable": True, "observation": {
            "observations": [{"tail": True}], "local_archive": {
                "schema": "dm-cp2077-camera-active-window/v1", "frozen": True,
                "enqueue_failures": 0, "enqueued_draws": 2}},
            "observation_archive": {"schema": "dm-cp2077-camera-archive/v2", "encoding": BLOCK_SCHEMA,
                "non_trainable": True, "durable": True, "file": "render-camera-draws.jsonl",
                "disk_budget_bytes": ARCHIVE_BYTES, "active_draw_capacity": 4096,
                "final_observations_include_archive": False, "archived_draws": 2,
                "bytes": len(block), "sha256": digest(block), "expanded_bytes": 2 * len(draw),
                "expanded_sha256": digest(draw * 2), "blocks": 1}}
        path.write_text(json.dumps(report), encoding="utf-8")
        return path, report, block, draw

    def test_exact_tokens_full_order_and_immutable_report(self):
        with tempfile.TemporaryDirectory() as root:
            path, _, block, draw = self.fixture(Path(root))
            self.assertEqual(decode_block(block), [draw, draw])
            before = path.read_bytes()
            result = read_camera_report(path)
            self.assertEqual(result["schema"], V3)
            self.assertEqual(result["observation"]["observations"], [json.loads(draw), json.loads(draw), {"tail": True}])
            self.assertEqual(path.read_bytes(), before)

    def test_corruption_missing_archive_and_expansion_are_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            path, report, block, _ = self.fixture(Path(root))
            for field in ("schema", "encoding", "expanded_sha256", "sha256", "blocks", "bytes", "archived_draws"):
                bad = copy.deepcopy(report); bad["observation_archive"][field] = None
                path.write_text(json.dumps(bad), encoding="utf-8")
                with self.assertRaises(ValueError):
                    read_camera_report(path)
            bad = copy.deepcopy(report); bad["observation_archive"]["expanded_bytes"] = EXPANDED_REPORT_BYTES
            path.write_text(json.dumps(bad), encoding="utf-8")
            with self.assertRaises(ValueError):
                read_camera_report(path)
            path.write_text(json.dumps(report), encoding="utf-8")
            path.with_name("render-camera-draws.jsonl").write_bytes(block[:-1])
            with self.assertRaises(ValueError):
                read_camera_report(path)
            del report["observation_archive"]
            path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaises(ValueError):
                read_camera_report(path)

    def test_compact_expansion_has_its_own_budget_and_includes_tail(self):
        with tempfile.TemporaryDirectory() as root:
            path, _, _, draw = self.fixture(Path(root))
            physical_bytes = path.stat().st_size
            # Scale the two ceilings down to exercise the same boundary without a large allocation.
            expansion = physical_bytes + len(draw) * 2
            with patch.multiple(camera_report, REPORT_BYTES=physical_bytes,
                                EXPANDED_REPORT_BYTES=expansion):
                self.assertEqual(len(read_camera_report(path)["observation"]["observations"]), 3)
            with patch.multiple(camera_report, REPORT_BYTES=physical_bytes,
                                EXPANDED_REPORT_BYTES=expansion - 1):
                with self.assertRaisesRegex(ValueError, "camera expansion exceeds bound"):
                    read_camera_report(path)

    def test_bad_references_schema_unused_values_and_duplicate_fields(self):
        with tempfile.TemporaryDirectory() as root:
            _, _, block, _ = self.fixture(Path(root))
            for change in (lambda b: b["rows"][0][0].__setitem__(1, 99),
                           lambda b: b["rows"][0][1].__setitem__(0, 0),
                           lambda b: b.__setitem__("schema", "future"),
                           lambda b: b["values"].append("unused")):
                bad = json.loads(block); change(bad)
                with self.assertRaises(ValueError):
                    decode_block(json.dumps(bad).encode())

    def test_v2_full_report_and_archive_still_cross_check(self):
        with tempfile.TemporaryDirectory() as root:
            path, report, _, draw = self.fixture(Path(root))
            path.with_name("render-camera-draws.jsonl").write_bytes(draw * 2)
            report["schema"] = V2
            a = report["observation_archive"]
            a.update(schema="dm-cp2077-camera-archive/v1", bytes=len(draw)*2, sha256=digest(draw*2),
                     final_observations_include_archive=True)
            report["observation"]["observations"] = [json.loads(draw), json.loads(draw), {"tail": True}]
            path.write_text(json.dumps(report), encoding="utf-8")
            self.assertEqual(len(read_camera_report(path)["observation"]["observations"]), 3)
            report["observation"]["observations"][0]["before"] = False
            path.write_text(json.dumps(report), encoding="utf-8")
            with self.assertRaises(ValueError):
                read_camera_report(path)
