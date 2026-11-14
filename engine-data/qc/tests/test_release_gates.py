"""两处「没检查却放行」的闸门回归。

1. **共视口径**：docs/72 修正单 001 把聚合改成严读，同时 trajgen 把 `covis_ratio`
   字段的语义一并改掉（宽读降级为 `covis_ratio_any_pair`）。改口径没改字段名，
   于是改严读之前的 `covis.json` 拿着一个大出一截的宽读数照样过闸——R2 交付桶里
   `2026_08_04_megacity_01` 就是这么过的（声明 0.686 / 严读 0.198）。

2. **video 工具链**：`ffprobe` 不在 PATH 时整组 video 规则静默 SKIP，判定仍是 PASS。
   本仓自带的静态 ffmpeg 就是只有 `ffmpeg` 没有 `ffprobe` 的分发，所以这不是假想情况。
"""

import contextlib
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: E402  (helpers 负责 sys.path 引导)

from engine_qc.checks_video import check_videos  # noqa: E402
from engine_qc.clip_checker import check_clip  # noqa: E402
from engine_qc.config import QCConfig  # noqa: E402
from engine_qc.loaders import discover  # noqa: E402
from engine_qc.report import UNVERIFIED  # noqa: E402


def small_cams(n=60):
    return {k: H.gen_cam_frames(n, [i * 2.0, 1.6, 0.0],
                                [1.0 if k == "1" else 0.0, 0.0,
                                 1.0 if k == "2" else 0.0],
                                yaw_rate=(i * 7.0), cam1=(k == "1"))
            for i, k in enumerate("1234")}


class CovisReadingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_covis_"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _report(self, covis):
        clip = H.write_mc_clip(self.tmp / "clip", small_cams(),
                               extra={"covis.json": covis})
        return check_clip(clip, "mc")

    def test_current_format_passes(self):
        r = self._report(H.covis_doc(0.62))
        self.assertEqual(H.statuses(r, "covis.reading"), {"PASS"})
        self.assertEqual(H.statuses(r, "covis.ratio_strict"), {"PASS"})

    def test_pre_strict_wide_file_is_rejected(self):
        """在库那条的形状：声明宽读数 0.686、严读只有 0.198、gate 自述宽读。

        三条规则都必须响：口径不自洽（reading）、严读不达标（ratio_strict）。
        `covis.ratio` 单看声明值仍然过线——这正是为什么不能只靠它。
        """
        doc = H.covis_doc(0.198, gate=H.WIDE_GATE, declared=0.686, any_pair=0.686)
        r = self._report(doc)
        self.assertEqual(H.statuses(r, "covis.reading"), {"FAIL"})
        self.assertEqual(H.statuses(r, "covis.ratio_strict"), {"FAIL"})
        self.assertEqual(H.statuses(r, "covis.ratio"), {"PASS"})
        self.assertEqual(r.verdict, "FAIL")

    def test_declaration_mismatch_alone_is_rejected(self):
        """数字够大但与曲线对不上——同样不予放行（口径不明即不放行）。"""
        doc = H.covis_doc(0.90, declared=0.55)
        r = self._report(doc)
        self.assertEqual(H.statuses(r, "covis.reading"), {"FAIL"})
        self.assertEqual(H.statuses(r, "covis.ratio_strict"), {"PASS"})

    def test_no_curves_falls_back_to_gate_prose(self):
        ok = self._report(H.covis_doc(0.62, with_curves=False))
        self.assertEqual(H.statuses(ok, "covis.reading"), {"PASS"})
        self.assertEqual(H.statuses(ok, "covis.ratio_strict"), {"SKIP"})

        bad = self._report(H.covis_doc(0.62, gate=H.WIDE_GATE, with_curves=False))
        self.assertEqual(H.statuses(bad, "covis.reading"), {"FAIL"})

    def test_no_curves_no_gate_is_rejected(self):
        doc = H.covis_doc(0.62, with_curves=False)
        doc.pop("gate")
        r = self._report(doc)
        self.assertEqual(H.statuses(r, "covis.reading"), {"FAIL"})

    def test_wrong_overlap_min_is_rejected(self):
        doc = H.covis_doc(0.62)
        doc["overlap_min"] = 0.15
        r = self._report(doc)
        self.assertEqual(H.statuses(r, "covis.reading"), {"FAIL"})


class VideoToolchainTest(unittest.TestCase):
    """有 mp4 却没有 ffprobe/ffmpeg → UNVERIFIED，不能是 PASS。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_vtool_"))
        # 整条合法 clip：这样判定里除 video 组外没有别的 FAIL，
        # UNVERIFIED 是不是唯一的非 PASS 来源才说得清。
        self.clip = H.write_mc_clip(self.tmp / "clip", H.valid_mc_cams(1800),
                                    extra={"covis.json": H.covis_doc(0.62)})
        for n in (1, 2, 3, 4):
            (self.clip / f"video_{n}.mp4").write_bytes(b"\x00" * 16)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_no_toolchain_marks_unverified(self):
        import engine_qc.checks_video as cv
        orig = cv.probe_tool
        cv.probe_tool = lambda: (None, None)
        try:
            results = check_videos(discover(self.clip, "mc"), "mc", QCConfig())
        finally:
            cv.probe_tool = orig
        self.assertEqual({r.status for r in results}, {UNVERIFIED})
        self.assertEqual(len(results), len(cv.VIDEO_RULES))

    def test_no_video_files_still_skips(self):
        """目录里根本没有 mp4 = json-only 自检，仍然 SKIP（不拦开发流）。"""
        for n in (1, 2, 3, 4):
            (self.clip / f"video_{n}.mp4").unlink()
        results = check_videos(discover(self.clip, "mc"), "mc", QCConfig())
        self.assertEqual({r.status for r in results}, {"SKIP"})

    def test_verdict_is_unverified_not_pass(self):
        import engine_qc.checks_video as cv
        orig = cv.probe_tool
        cv.probe_tool = lambda: (None, None)
        try:
            report = check_clip(self.clip, "mc")
        finally:
            cv.probe_tool = orig
        self.assertEqual(report.failures(), [])
        self.assertEqual(report.verdict, UNVERIFIED)
        self.assertEqual(report.video_verdict(), UNVERIFIED)


class FfmpegFallbackParseTest(unittest.TestCase):
    """`ffmpeg -i` 的 stderr → ffprobe 形状。映射按 ffmpeg 自己的语义：
    `fps`→avg_frame_rate、`tbr`→r_frame_rate，CFR 判据要的正是这两个。"""

    SAMPLE = (
        "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'video_1.mp4':\n"
        "  Duration: 00:01:00.00, start: 0.000000, bitrate: 12554 kb/s\n"
        "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), "
        "yuv420p(progressive), 1920x1080, 12352 kb/s, 30 fps, 30 tbr, 15360 tbn (default)\n"
        "  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), "
        "48000 Hz, stereo, fltp, 193 kb/s (default)\n"
    )

    VOLUME = (
        "[Parsed_volumedetect_0 @ 0x600002d0] n_samples: 5760000\n"
        "[Parsed_volumedetect_0 @ 0x600002d0] mean_volume: -20.3 dB\n"
        "[Parsed_volumedetect_0 @ 0x600002d0] max_volume: -3.5 dB\n"
    )

    @contextlib.contextmanager
    def _fake_ffmpeg(self):
        """只替 `subprocess.run`，让真正的解析器端到端跑一遍。

        check_videos 现在对每个 mp4 问 ffmpeg 两次：`-i` 探测流、
        `volumedetect` 测响度——按命令行分别喂样本。
        """
        import subprocess

        def fake_run(cmd, *a, **k):
            class R:
                stdout = ""
                returncode = 0
            R.stderr = (FfmpegFallbackParseTest.VOLUME if "volumedetect" in cmd
                        else FfmpegFallbackParseTest.SAMPLE)
            return R()

        orig = subprocess.run
        subprocess.run = fake_run
        try:
            yield
        finally:
            subprocess.run = orig

    def test_parses_streams(self):
        import engine_qc.checks_video as cv
        with self._fake_ffmpeg():
            info = cv._probe_ffmpeg("ffmpeg", Path("video_1.mp4"))
        self.assertIsNotNone(info)
        v = [s for s in info["streams"] if s["codec_type"] == "video"][0]
        a = [s for s in info["streams"] if s["codec_type"] == "audio"]
        self.assertEqual(v["codec_name"], "h264")
        self.assertEqual((v["width"], v["height"]), (1920, 1080))
        # ffmpeg 原样打印的是 `30 fps, 30 tbr`——保留原串，让证据行能对回工具输出
        self.assertEqual(v["avg_frame_rate"], "30/1")
        self.assertEqual(v["r_frame_rate"], "30/1")
        self.assertAlmostEqual(float(info["format"]["duration"]), 60.0)
        self.assertEqual(len(a), 1)

    def test_rules_pass_through_fallback(self):
        import engine_qc.checks_video as cv
        tmp = Path(tempfile.mkdtemp(prefix="qc_vfb_"))
        try:
            clip = H.write_mc_clip(tmp / "clip", small_cams())
            for n in (1, 2, 3, 4):
                (clip / f"video_{n}.mp4").write_bytes(b"\x00" * 16)
            orig_tool = cv.probe_tool
            cv.probe_tool = lambda: ("ffmpeg", "/usr/bin/ffmpeg")
            try:
                with self._fake_ffmpeg():
                    results = check_videos(discover(clip, "mc"), "mc", QCConfig())
            finally:
                cv.probe_tool = orig_tool
            self.assertEqual({r.status for r in results}, {"PASS"})
            self.assertTrue(all("回退解析" in r.evidence for r in results))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
