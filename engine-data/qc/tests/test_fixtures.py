"""需求方样例镜像（engine-data/fixtures）按对应 mode 通过 schema/键名/数值组。

样例只有 1–3 帧，时长/物理组 FAIL/SKIP 是预期——断言用规则粒度而非整 clip 全绿。
"""

import json
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.clip_checker import check_clip


class FixtureMCTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_fix_mc_"))
        cam1 = json.loads((H.FIXTURES / "mc_cam1_action_camera.sample.json").read_text())
        camN = json.loads((H.FIXTURES / "mc_camN_action_camera.sample.json").read_text())
        sysinfo = json.loads((H.FIXTURES / "systeminfo.sample.json").read_text())
        clip = cls.tmp / "scene-orbit-42-0001"
        clip.mkdir()
        (clip / "action_camera_1.json").write_text(json.dumps(cam1))
        for n in (2, 3, 4):
            (clip / f"action_camera_{n}.json").write_text(json.dumps(camN))
        for n in (1, 2, 3, 4):
            (clip / f"systeminfo_{n}.json").write_text(json.dumps(sysinfo))
        cls.report = check_clip(clip, "mc")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_container_schema_keys_pass(self):
        for rule in ("json.container", "json.schema", "json.keys"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_numeric_group_pass(self):
        self.assertEqual(H.statuses(self.report, "num.quat_norm"), {"PASS"})
        self.assertEqual(H.statuses(self.report, "num.fx_fy"), {"PASS"})
        self.assertEqual(H.statuses(self.report, "num.intrinsics_const"), {"PASS"})
        # cam1 必含 metric_scale=1.0 PASS；cam2..4 非必含 → SKIP
        self.assertEqual(
            H.status_for(self.report, "num.metric_scale", "action_camera_1.json"), "PASS")
        self.assertEqual(
            H.status_for(self.report, "num.metric_scale", "action_camera_2.json"), "SKIP")

    def test_sysinfo_pass(self):
        for rule in ("sysinfo.schema", "sysinfo.origin", "sysinfo.resolution"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_duration_fails_as_expected(self):
        # 样例只有 1 帧，时长组 FAIL 是预期（规则粒度断言）
        self.assertEqual(H.statuses(self.report, "dur.frames"), {"FAIL"})

    def test_video_and_covis_skipped(self):
        self.assertEqual(H.statuses(self.report, "pack.video"), {"SKIP"})
        self.assertEqual(H.statuses(self.report, "covis.ratio"), {"SKIP"})
        self.assertEqual(H.statuses(self.report, "cam4.switch_count"), {"SKIP"})


class FixtureSFMTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_fix_sfm_"))
        f15 = json.loads((H.FIXTURES / "sfm_action_camera_15fps.sample.json").read_text())
        # 30fps 文件用 15fps 样例内容合成：frame 改连续 0..5、去 depth_name
        f30 = []
        for i in range(2 * len(f15)):
            src = dict(f15[i // 2])
            src.pop("depth_name", None)
            src["frame"] = i
            f30.append(src)
        sysinfo = json.loads((H.FIXTURES / "systeminfo.sample.json").read_text())
        clip = cls.tmp / "scene-roam-7-0001"
        clip.mkdir()
        (clip / "action_camera_30fps.json").write_text(json.dumps(f30))
        (clip / "action_camera_15fps.json").write_text(json.dumps(f15))
        (clip / "systeminfo.json").write_text(json.dumps(sysinfo))
        cls.report = check_clip(clip, "sfm")

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_container_schema_keys_pass(self):
        for rule in ("json.container", "json.schema", "json.keys"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_numeric_group_pass(self):
        for rule in ("num.quat_norm", "num.fx_fy", "num.intrinsics_const",
                     "num.metric_scale"):
            self.assertEqual(H.statuses(self.report, rule), {"PASS"}, rule)

    def test_15fps_step_and_depth_name(self):
        self.assertEqual(
            H.status_for(self.report, "frame.continuity", "action_camera_15fps.json"),
            "PASS")
        self.assertEqual(H.statuses(self.report, "frame.depthname"), {"PASS"})

    def test_pair_consistency(self):
        self.assertEqual(H.statuses(self.report, "sync.sfm_pair"), {"PASS"})

    def test_depth_zip_skipped(self):
        self.assertEqual(H.statuses(self.report, "depth.match"), {"SKIP"})

    def test_duration_fails_as_expected(self):
        self.assertEqual(H.statuses(self.report, "dur.frames"), {"FAIL"})


if __name__ == "__main__":
    unittest.main()
