"""worldline_qc 全链路：合成组 + ffmpeg 生成的小视频 → check_group → HTML/JSON/CLI 退出码。

ffmpeg 缺失时跳过；ENGINE_QC_REQUIRE_FFMPEG=1（CI）时缺 ffmpeg 直接失败，防止「测试存在但从不跑」。
像素面依赖 opencv：装了就断言像素规则出结果，没装就断言它们是 UNVERIFIED 而不是 PASS。
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: F401
import wl_helpers as W

from worldline_qc.cli import main
from worldline_qc.html_report import render_html
from worldline_qc.model import FAIL, PASS, UNVERIFIED
from worldline_qc.pipeline import check_group

FFMPEG = shutil.which("ffmpeg")
REQUIRE = os.environ.get("ENGINE_QC_REQUIRE_FFMPEG") == "1"
try:
    import cv2  # noqa: F401
    import numpy  # noqa: F401
    HAVE_CV = True
except (ImportError, OSError, RuntimeError):
    HAVE_CV = False


def _make_video(path: Path, n_frames: int) -> bool:
    cmd = [FFMPEG, "-hide_banner", "-loglevel", "error", "-y",
           "-f", "lavfi", "-i", f"testsrc=size=1920x1080:rate=30",
           "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
           "-frames:v", str(n_frames), "-c:v", "libx264", "-preset", "ultrafast", "-crf", "35", "-pix_fmt", "yuv420p",
           "-c:a", "aac", "-shortest", str(path)]
    return subprocess.run(cmd, capture_output=True).returncode == 0


class PipelineTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if FFMPEG is None:
            if REQUIRE:
                raise AssertionError("ENGINE_QC_REQUIRE_FFMPEG=1 但找不到 ffmpeg")
            raise unittest.SkipTest("无 ffmpeg")
        cls.tmp = Path(tempfile.mkdtemp())
        cls.video = cls.tmp / "v.mp4"
        if not _make_video(cls.video, 150):
            raise unittest.SkipTest("ffmpeg 无法生成测试视频（缺 libx264/lavfi）")
        cls.root = W.write_group(cls.tmp / "2026_09_24_beach_001", n=150, videos=cls.video, distinct_clock=True,
                                 c_text=W.C_TEXT_WITH_SEMANTICS, orbit_is_six_dir=True)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(getattr(cls, "tmp", ""), ignore_errors=True)

    def test_full_group(self):
        rep, art = check_group(self.root, expect_s=5.0, pixels=True, n_thumbs=2)
        by = {(r.rule, r.target): r for r in rep.results}
        self.assertEqual(by[("pack.files", "group")].status, PASS)
        self.assertEqual(by[("video.duration", "AB/1")].status, PASS, by[("video.duration", "AB/1")].evidence)
        self.assertEqual(by[("video.fps_cfr", "AB/1")].status, PASS)
        self.assertEqual(by[("video.json_frames", "C/3")].status, PASS)
        self.assertEqual(by[("sysinfo.resolution", "D/4")].status, PASS)
        self.assertEqual(by[("cam.focal", "AB/1")].status, PASS, by[("cam.focal", "AB/1")].evidence)
        self.assertEqual(by[("sync.time", "世界线C")].status, PASS)
        self.assertEqual(by[("sync.person_lanes", "世界线D")].status, PASS)
        self.assertEqual(by[("wl.time_real_clock", "group")].status, PASS)
        if HAVE_CV:
            self.assertIn(by[("pix.motion_ratio", "AB/1")].status, ("WARN",))
            self.assertEqual(by[("pix.luminance", "AB/1")].status, PASS)
            self.assertTrue(art["thumbs"]["AB/1"])
            self.assertIn("pix.xtake_pre", {r.rule for r in rep.results})
        else:
            self.assertEqual(by[("pix.motion_ratio", "AB/1")].status, UNVERIFIED)
            self.assertEqual(rep.verdict, UNVERIFIED if not any(r.status == FAIL for r in rep.results) else FAIL)
        html = render_html(rep, art)
        self.assertIn(rep.group, html)
        for cid in ("R01", "R16", "R23", "R34", "R42"):
            self.assertIn(cid, html)
        self.assertIn("需求条款", html)

    def test_cli_exit_codes_and_outputs(self):
        out_json = self.tmp / "out.json"
        out_html = self.tmp / "out.html"
        code = main(["check-group", str(self.root), "--expect-duration-s", "5", "--no-pixels",
                     "--json", str(out_json), "--html", str(out_html), "--quiet"])
        self.assertIn(code, (1, 2))                     # 遍布局 → pack.layout FAIL（1）；像素关闭 → UNVERIFIED（2）
        d = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(d["group"], self.root.name)
        self.assertIn("results", d)
        self.assertTrue(out_html.stat().st_size > 10_000)
        self.assertEqual(d["verdict"], FAIL)             # 布局 FAIL 压过 UNVERIFIED


if __name__ == "__main__":
    unittest.main()
