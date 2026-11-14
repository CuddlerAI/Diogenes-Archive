"""video.audio 响度检测：纯静音轨必须 FAIL（69 号 §8.1「有声音」）。

2026-08-07 评审 R2 交付 `2MultiCamera/OasisScene/0807/2026_08_07_shamo_01`：
4 路 mp4 都含 aac 流、volumedetect mean/max 全是 -91.0 dB（数字静音），当时的
video.audio 只查「存在音频流」于是判 PASS。本组测试钉住四件事：

1. 静音轨 FAIL「音轨为静音」；
2. 有声轨 PASS，证据里带实测 dB；
3. 测不了（只有 ffprobe 没有 ffmpeg / volumedetect 无输出）→ UNVERIFIED，
   不降级回存在性检查；
4. 阈值边界：恰好等于 audio_silence_max_db 不判静音（「低于」才 FAIL）。

单元组用假 subprocess 钉规则逻辑；集成组用真 ffmpeg 现场编码 fixture
（anullsrc 静音 / aevalsrc 正弦有声 / 无音轨），走完整探测→响度链路。
CI 里 ubuntu runner 不预装 ffmpeg——workflow 显式安装并设
ENGINE_QC_REQUIRE_FFMPEG=1：该环境变量在场时集成组**不许** skip，
装失败会红掉而不是静默变绿（「测试存在但 CI 从不跑」的老坑）。
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: F401  (sys.path 引导)

import engine_qc.checks_video as cv
from engine_qc.checks_video import check_videos
from engine_qc.config import QCConfig
from engine_qc.loaders import discover
from engine_qc.report import FAIL, PASS, UNVERIFIED

# ffmpeg -i 探测样本（含 aac 音频流）
PROBE_TEXT = (
    "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'video_1.mp4':\n"
    "  Duration: 00:01:00.00, start: 0.000000, bitrate: 12554 kb/s\n"
    "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), "
    "yuv420p(progressive), 1920x1080, 12352 kb/s, 30 fps, 30 tbr, 15360 tbn (default)\n"
    "  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), "
    "48000 Hz, stereo, fltp, 193 kb/s (default)\n"
)
PROBE_TEXT_NO_AUDIO = "\n".join(
    ln for ln in PROBE_TEXT.splitlines() if "Audio:" not in ln) + "\n"

# ffprobe -print_format json 探测样本（含 aac 音频流）
PROBE_JSON = json.dumps({
    "streams": [
        {"codec_type": "video", "codec_name": "h264", "width": 1920, "height": 1080,
         "r_frame_rate": "30/1", "avg_frame_rate": "30/1", "duration": "60.0"},
        {"codec_type": "audio", "codec_name": "aac"},
    ],
    "format": {"duration": "60.0"},
})


def volume_stderr(max_db, mean_db):
    return (
        "[Parsed_volumedetect_0 @ 0x600002d0] n_samples: 5760000\n"
        f"[Parsed_volumedetect_0 @ 0x600002d0] mean_volume: {mean_db} dB\n"
        f"[Parsed_volumedetect_0 @ 0x600002d0] max_volume: {max_db} dB\n"
    )


VOLUME_SILENT = volume_stderr(-91.0, -91.0)   # shamo_01 实测值
VOLUME_LOUD = volume_stderr(-3.5, -20.3)


class AudioLoudnessUnitTest(unittest.TestCase):
    """假 subprocess 钉规则逻辑（真编码链路见下面的集成组）。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_audio_"))
        self.clip = self.tmp / "clip"
        self.clip.mkdir()
        (self.clip / "video_1.mp4").write_bytes(b"\x00" * 16)

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _results(self, *, vol_stderr=VOLUME_LOUD, tool="ffmpeg",
                 probe_stderr=PROBE_TEXT, no_ffmpeg=False, cfg=None):
        def fake_run(cmd, *a, **k):
            class R:
                returncode = 0
                stdout = ""
                stderr = ""
            if "volumedetect" in cmd:
                R.stderr = vol_stderr
            elif tool == "ffprobe":
                R.stdout = PROBE_JSON
            else:
                R.stderr = probe_stderr
            return R()

        orig_tool, orig_ffmpeg, orig_run = cv.probe_tool, cv.ffmpeg_path, subprocess.run
        cv.probe_tool = lambda: (tool, f"/usr/bin/{tool}")
        cv.ffmpeg_path = (lambda: None) if no_ffmpeg else (lambda: "/usr/bin/ffmpeg")
        subprocess.run = fake_run
        try:
            return check_videos(discover(self.clip, "mc"), "mc", cfg or QCConfig())
        finally:
            subprocess.run = orig_run
            cv.probe_tool, cv.ffmpeg_path = orig_tool, orig_ffmpeg

    def _audio(self, results):
        rs = [r for r in results if r.rule == "video.audio"]
        self.assertEqual(len(rs), 1)
        return rs[0]

    def test_digital_silence_fails(self):
        r = self._audio(self._results(vol_stderr=VOLUME_SILENT))
        self.assertEqual(r.status, FAIL)
        self.assertIn("音轨为静音", r.evidence)
        self.assertIn("-91.0", r.evidence)

    def test_loud_track_passes_with_db_evidence(self):
        r = self._audio(self._results(vol_stderr=VOLUME_LOUD))
        self.assertEqual(r.status, PASS)
        self.assertIn("max_volume -3.5 dB", r.evidence)

    def test_threshold_is_strictly_below(self):
        """恰好 -70.0 不算静音（判据是「低于」）；-70.1 算。"""
        at = self._audio(self._results(vol_stderr=volume_stderr(-70.0, -75.0)))
        self.assertEqual(at.status, PASS)
        below = self._audio(self._results(vol_stderr=volume_stderr(-70.1, -75.0)))
        self.assertEqual(below.status, FAIL)

    def test_ffprobe_only_is_unverified_not_pass(self):
        """只有 ffprobe：流存在查得出、响度测不了 → UNVERIFIED，其余规则照常。"""
        results = self._results(tool="ffprobe", no_ffmpeg=True)
        r = self._audio(results)
        self.assertEqual(r.status, UNVERIFIED)
        self.assertIn("响度未测", r.evidence)
        others = {x.status for x in results if x.rule != "video.audio"}
        self.assertEqual(others, {PASS})

    def test_volumedetect_no_output_is_unverified(self):
        r = self._audio(self._results(vol_stderr=""))
        self.assertEqual(r.status, UNVERIFIED)
        self.assertIn("volumedetect", r.evidence)

    def test_missing_audio_stream_still_fails(self):
        r = self._audio(self._results(probe_stderr=PROBE_TEXT_NO_AUDIO))
        self.assertEqual(r.status, FAIL)
        self.assertEqual(r.evidence, "无音轨")


FFMPEG = shutil.which("ffmpeg")
#: CI 设 1：此时缺 ffmpeg / 编码失败按测试失败处理，禁止静默 skip。
REQUIRE_FFMPEG = os.environ.get("ENGINE_QC_REQUIRE_FFMPEG") == "1"


@unittest.skipUnless(FFMPEG or REQUIRE_FFMPEG,
                     "PATH 无 ffmpeg，跳过真编码集成组（CI 以 ENGINE_QC_REQUIRE_FFMPEG=1 禁跳）")
class AudioLoudnessEncodeTest(unittest.TestCase):
    """真 ffmpeg 编码三个 1s fixture，走完整探测→volumedetect 链路。"""

    @classmethod
    def setUpClass(cls):
        if FFMPEG is None:
            raise AssertionError(
                "ENGINE_QC_REQUIRE_FFMPEG=1 但 PATH 里没有 ffmpeg——CI 的 ffmpeg 安装步骤坏了")
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_audio_enc_"))

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def _encode_clip(self, name, audio_src):
        """1s 小视频（视频/音频编码器用 ffmpeg 对 mp4 的默认选择）。"""
        clip = self.tmp / name
        clip.mkdir()
        cmd = [FFMPEG, "-hide_banner", "-loglevel", "error",
               "-f", "lavfi", "-i", "testsrc=duration=1:size=160x120:rate=30"]
        if audio_src:
            cmd += ["-f", "lavfi", "-i", audio_src, "-shortest"]
        cmd += ["-pix_fmt", "yuv420p", "-y", str(clip / "video_1.mp4")]
        res = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if res.returncode != 0:
            msg = f"本机 ffmpeg 编码 fixture 失败：{res.stderr.strip()[-300:]}"
            if REQUIRE_FFMPEG:
                raise AssertionError(msg)
            raise unittest.SkipTest(msg)
        return clip

    def _audio_result(self, clip):
        results = check_videos(discover(clip, "mc"), "mc", QCConfig())
        rs = [r for r in results if r.rule == "video.audio"]
        self.assertEqual(len(rs), 1)
        return rs[0]

    def test_silent_track_fails(self):
        clip = self._encode_clip("silent", "anullsrc=r=48000:cl=mono")
        r = self._audio_result(clip)
        self.assertEqual(r.status, FAIL, r.evidence)
        self.assertIn("音轨为静音", r.evidence)

    def test_voiced_track_passes(self):
        # 0.5 幅度正弦 ≈ -6 dBFS，远高于 -70 阈值
        clip = self._encode_clip("voiced", "aevalsrc=0.5*sin(440*2*PI*t):s=48000:d=1")
        r = self._audio_result(clip)
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("max_volume", r.evidence)

    def test_no_audio_track_fails(self):
        clip = self._encode_clip("mute", None)
        r = self._audio_result(clip)
        self.assertEqual(r.status, FAIL, r.evidence)
        self.assertEqual(r.evidence, "无音轨")


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
