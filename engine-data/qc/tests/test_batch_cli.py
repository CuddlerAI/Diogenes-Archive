"""check-batch 抽检协议与 CLI 出入口（退出码 / --json）。"""

import contextlib
import io
import json
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H

from engine_qc.batch import check_batch, render_batch_text
from engine_qc.cli import main as cli_main


class BatchTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_batch_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_accept_batch(self):
        batch = self.tmp / "batch"
        H.write_mc_clip(batch / "scene-orbit-1-0001", H.valid_mc_cams(1800))
        H.write_mc_clip(batch / "scene-orbit-2-0002", H.valid_mc_cams(1800))
        br = check_batch(batch, "mc", seed=7)
        self.assertEqual(br.protocol["verdict"], "ACCEPT")
        self.assertEqual(br.verdict, "PASS")
        s = br.summary()
        self.assertEqual((s["clips_total"], s["clips_fail"]), (2, 0))
        # 无 ffprobe 可用视频 → video 闸门 N/A 不应打回
        self.assertIn(br.protocol["video_gate"], ("PASS", "N/A（无视频文件）"))
        text = render_batch_text(br)
        self.assertIn("协议判定：ACCEPT", text)

    def test_reject_batch_when_sampled_json_both_fail(self):
        batch = self.tmp / "batch"
        # 两条坏 clip（时长不足 60s）→ 抽 2 条 json 双不过 → 整包打回
        H.write_mc_clip(batch / "scene-orbit-1-0001", H.valid_mc_cams(90))
        H.write_mc_clip(batch / "scene-orbit-2-0002", H.valid_mc_cams(90))
        br = check_batch(batch, "mc", seed=7)
        self.assertEqual(br.protocol["json_gate"], "REJECT")
        self.assertEqual(br.protocol["verdict"], "REJECT")
        self.assertEqual(br.verdict, "FAIL")

    def test_empty_batch_rejects(self):
        batch = self.tmp / "empty"
        batch.mkdir()
        br = check_batch(batch, "mc")
        self.assertEqual(br.protocol["verdict"], "REJECT")
        self.assertEqual(br.verdict, "FAIL")


class CliTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_cli_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _run(self, argv):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = cli_main(argv)
        return code, buf.getvalue()

    def test_check_clip_exit_codes(self):
        good = H.write_mc_clip(self.tmp / "good", H.valid_mc_cams(1800))
        code, out = self._run(["check-clip", str(good), "--mode", "mc"])
        self.assertEqual(code, 0, out)
        self.assertIn("[PASS]", out)

        bad_cams = H.valid_mc_cams(90)
        for f in bad_cams["2"]:
            f["camera_follow_offset"] = f.pop("camera_Follow Offset")
        bad = H.write_mc_clip(self.tmp / "bad", bad_cams)
        code, out = self._run(["check-clip", str(bad), "--mode", "mc"])
        self.assertEqual(code, 1)
        self.assertIn("[FAIL]", out)
        self.assertIn("json.keys", out)

    def test_check_clip_json_output(self):
        good = H.write_mc_clip(self.tmp / "good", H.valid_mc_cams(1800))
        code, out = self._run(["check-clip", str(good), "--mode", "mc", "--json"])
        self.assertEqual(code, 0)
        doc = json.loads(out)
        self.assertEqual(doc["verdict"], "PASS")
        self.assertTrue(any(r["rule"] == "decouple.pairs" and r["status"] == "PASS"
                            for r in doc["results"]))

    def test_check_batch_json_output(self):
        batch = self.tmp / "batch"
        H.write_mc_clip(batch / "c1", H.valid_mc_cams(1800))
        code, out = self._run(["check-batch", str(batch), "--mode", "mc", "--json"])
        self.assertEqual(code, 0, out)
        doc = json.loads(out)
        self.assertEqual(doc["protocol"]["verdict"], "ACCEPT")

    def test_init_frame_thresholds_configurable(self):
        cams = H.valid_mc_cams(1800)
        good = H.write_mc_clip(self.tmp / "clip", cams)
        # 收紧内控值到 1m 会让 2.83m 的初始间距 FAIL
        code, out = self._run(["check-clip", str(good), "--mode", "mc",
                               "--init-dist-m", "1.0"])
        self.assertEqual(code, 1)
        self.assertIn("init.distance", out)


if __name__ == "__main__":
    unittest.main()
