"""depth 内容组规则测试（docs/83 §5）。

关键的一条是**负例**：跨模态反投影自检必须对人为注入的 ±1 帧错位判 FAIL。
一条只会说 PASS 的规则等于没有规则——D2 的验收判据就写着这一条。
"""

import math
import shutil
import tempfile
import unittest
import zipfile
from pathlib import Path

import depth_fixture as DF
import helpers as H

from engine_qc import checks_depth as cd
from engine_qc import exr
from engine_qc.config import QCConfig
from engine_qc.report import FAIL, PASS, SKIP

W, H_ = 128, 72          # 小图跑得快；格式组按 cfg 覆写成同尺寸
FX = FY = 64.0
CX, CY = W / 2.0, H_ / 2.0


def _cfg():
    c = QCConfig()
    c.depth_width, c.depth_height = W, H_
    c.reproject_points = 1500
    c.reproject_min_samples = 50
    return c


def _frame(i, pos, quat, name):
    return {
        "time": "2026-08-04 10:00:00.000", "fps": 30.0, "frame": i,
        "depth_name": name,
        "camera_position": list(pos),
        "camera_rotation_quaternion": list(quat),
        "camera_follow_offset": [0.0, 0.0, 0.0],
        # json 内参按 1080p 视频给；depth 是 W x H_，QC 按分辨率比缩放（docs/75 问题 10）
        "camera_intrinsics": {"fx": FX * (1920.0 / W), "fy": FY * (1920.0 / W),
                              "cx": CX * (1920.0 / W), "cy": CY * (1920.0 / W)},
        "camera_speed": [0.0, 0.0, 0.0],
        "player_position": list(pos),
        "player_rotation_quaternion": list(quat),
        "player_speed": [0.0, 0.0, 0.0],
        "mouseX": 0.5, "mouseY": 0.5, "mouse_dx": 0.0, "mouse_dy": 0.0,
        "keyCode": [], "metric_scale": 1.0,
    }


SYSINFO = {"gameProcessName": "synthetic", "x": 0, "y": 0,
           "width": 1920, "height": 1080, "recordDpi": 1.0}


def build_clip(tmp: Path, n_depth=5, dz=0.6, compression=DF.COMP_ZIP,
               shift_depth=0, channel="Y", pixel_type=1, nested=False,
               ext=".exr", accel=0.25, yaw_rate=3.0, vflip=False,
               quat_fn=None):
    """造一条自洽的合成 clip：相机绕看球 + 地面，带加速度与转动。

    **必须带加速度/转动**：匀速直线运动下 pose_a→pose_b 与 pose_{a+1}→pose_{b+1}
    完全相同，整体错一帧不改变相对位姿，depth.align 在几何上无判别力
    （见 test_constant_velocity_is_unobservable）。68 号 §3.2 要求的运镜扰动
    （单方向移动不得持续 2s 以上）意味着真实交付数据天然带这种变化。

    shift_depth != 0 时把 depth 与 json 帧**人为错位**，用于负例。
    vflip=True 时把渲染好的深度图**按行上下镜像**后写入——模拟 docs/88 的
    sfm-d1-20260805 缺陷（DepthSink 没对 bottom-up readback 做翻转）。
    quat_fn(i) 可覆写相机姿态（如注入横滚，验证 depth.orientation 的横滚门槛）。
    """
    poses = []
    for i in range(n_depth + 2):
        z = -20.0 + i * dz + accel * i * i          # 变速
        quat = quat_fn(i) if quat_fn else DF.yaw_quat(yaw_rate * i)
        poses.append(((0.0, 0.0, z), quat))
    frames15 = []
    zpath = tmp / "depth.zip"
    with zipfile.ZipFile(zpath, "w", zipfile.ZIP_STORED) as zf:
        for i in range(n_depth):
            name = f"depth_{i * 2}"
            pos, quat = poses[i]
            src = poses[i + shift_depth] if 0 <= i + shift_depth < len(poses) else poses[i]
            px = DF.render_depth(W, H_, src[0], src[1], FX, FY, CX, CY)
            if vflip:
                px = [v for y in range(H_ - 1, -1, -1)
                      for v in px[y * W:(y + 1) * W]]
            arc = name + ext
            if nested:
                arc = "depth/" + arc
            zf.writestr(arc, DF.write_exr_bytes(W, H_, px, channel=channel,
                                                compression=compression,
                                                pixel_type=pixel_type))
            frames15.append(_frame(i * 2, pos, quat, name))
    return zpath, frames15


class DepthFlatTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_depth_flat_"))

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self):
        self.d = Path(tempfile.mkdtemp(dir=self.tmp))

    def test_flat_passes(self):
        z, _ = build_clip(self.d, n_depth=2)
        self.assertEqual(cd.check_flat(z).status, PASS)

    def test_nested_directory_fails(self):
        z, _ = build_clip(self.d, n_depth=2, nested=True)
        r = cd.check_flat(z)
        self.assertEqual(r.status, FAIL)
        self.assertIn("嵌套", r.evidence)

    def test_wrong_extension_fails(self):
        z, _ = build_clip(self.d, n_depth=2, ext=".png")
        r = cd.check_flat(z)
        self.assertEqual(r.status, FAIL)
        self.assertIn(".exr", r.evidence)

    def test_missing_zip_skips(self):
        self.assertEqual(cd.check_flat(None).status, SKIP)


class DepthContentTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="qc_depth_"))
        cls.cfg = _cfg()
        cls.z, cls.frames = build_clip(cls.tmp, n_depth=5)
        cls.res = {r.rule: r for r in
                   cd.check_depth_content(cls.z, cls.frames, SYSINFO, cls.cfg)}

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_format_passes(self):
        r = self.res[cd.DEPTH_RULE_FORMAT]
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("HALF", r.evidence)

    def test_quantize_passes(self):
        r = self.res[cd.DEPTH_RULE_QUANT]
        self.assertEqual(r.status, PASS, r.evidence)

    def test_nan_reports_band(self):
        r = self.res[cd.DEPTH_RULE_NAN]
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("NaN 占比", r.evidence)

    def test_range_passes(self):
        r = self.res[cd.DEPTH_RULE_RANGE]
        self.assertEqual(r.status, PASS, r.evidence)

    def test_reproject_passes_on_consistent_clip(self):
        r = self.res[cd.DEPTH_RULE_REPROJ]
        self.assertEqual(r.status, PASS, r.evidence)

    def test_alignment_passes_on_consistent_clip(self):
        r = self.res[cd.DEPTH_RULE_ALIGN]
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("偏移 0 残差最小", r.evidence)

    def test_sampling_coverage_is_reported(self):
        # 「no silent caps」：抽了多少帧必须写在证据里
        self.assertIn("抽样", self.res[cd.DEPTH_RULE_FORMAT].evidence)

    def test_orientation_passes_on_upright_clip(self):
        r = self.res[cd.DEPTH_RULE_ORIENT]
        self.assertEqual(r.status, PASS, r.evidence)


class DepthOrientationTest(unittest.TestCase):
    """docs/88：上下翻转落在 reproject/align 的几何盲区里——纯 yaw + 水平移动下，
    镜像与运动可交换，帧间自洽性不受影响（sfm-d1-20260805 的 900 帧全翻转就是
    这么在 58 条 PASS 下漏过的）。方向必须由内容先验单独抓。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_depth_orient_"))
        self.cfg = _cfg()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _run(self, **kw):
        d = Path(tempfile.mkdtemp(dir=self.tmp))
        z, frames = build_clip(d, **kw)
        return {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}

    def test_vertical_flip_fails_while_reproject_stays_blind(self):
        """负例主判据：全帧行序镜像必须被 orientation 抓 FAIL；
        同时印证盲区确实存在——reproject 在同一份翻转数据上不报错。"""
        res = self._run(n_depth=5, vflip=True)
        r = res[cd.DEPTH_RULE_ORIENT]
        self.assertEqual(r.status, FAIL, r.evidence)
        self.assertIn("翻转", r.evidence)
        self.assertNotEqual(res[cd.DEPTH_RULE_REPROJ].status, FAIL,
                            "反投影对垂直镜像应当无感（盲区），否则本规则就多余了")

    def test_pitched_down_clip_decides_by_band_medians(self):
        """俯视无天空（NaN 证据缺席）时，顶/底带有效深度中位必须还能判向。"""
        pitch = [math.sin(math.radians(35) / 2), 0.0, 0.0, math.cos(math.radians(35) / 2)]
        r = self._run(n_depth=5, quat_fn=lambda i: list(pitch))[cd.DEPTH_RULE_ORIENT]
        self.assertEqual(r.status, PASS, r.evidence)
        self.assertIn("带中位票", r.evidence)
        r2 = self._run(n_depth=5, quat_fn=lambda i: list(pitch),
                       vflip=True)[cd.DEPTH_RULE_ORIENT]
        self.assertEqual(r2.status, FAIL, r2.evidence)

    def test_rolled_camera_skips_with_reason(self):
        """横滚把「图像上下」转离「世界上下」，行带统计失去意义——必须 SKIP 并说明。"""
        roll = [0.0, 0.0, math.sin(math.radians(15)), math.cos(math.radians(15))]  # 30° 横滚
        r = self._run(n_depth=5, quat_fn=lambda i: list(roll))[cd.DEPTH_RULE_ORIENT]
        self.assertEqual(r.status, SKIP, r.evidence)
        self.assertIn("横滚", r.evidence)


class DepthNegativeTest(unittest.TestCase):
    """每条规则都要有能让它变红的输入。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_depth_neg_"))
        self.cfg = _cfg()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _run(self, **kw):
        z, frames = build_clip(self.tmp, **kw)
        return {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}

    def test_frame_shift_of_one_fails_reprojection(self):
        """D2 验收判据：人为注入 ±1 帧错位必须 FAIL。

        这正是 ReadbackRing 修复前那条「Update 期发请求抓到上一帧」缺陷的形状
        （docs/78 §9 修正 2）——文件名对账永远抓不到它。
        """
        res = self._run(n_depth=6, shift_depth=1)
        r = res[cd.DEPTH_RULE_ALIGN]
        self.assertEqual(r.status, FAIL, r.evidence)
        self.assertIn("整体错位", r.evidence)

    def test_frame_shift_of_minus_one_fails_alignment(self):
        res = self._run(n_depth=6, shift_depth=-1)
        r = res[cd.DEPTH_RULE_ALIGN]
        self.assertEqual(r.status, FAIL, r.evidence)
        self.assertIn("整体错位", r.evidence)

    def test_weak_margin_at_zero_skips_not_fails(self):
        """argmin=0 但优势拉不开：测不了 ≠ 不合格（docs/88 辩题二十三）。

        慢速/近匀速 clip 下对齐与错位的残差同样挤在一起，此时 FAIL 会把整类
        慢镜头轨迹（sfm-d1 的 Loop-有位移 首当其冲）判成永不可交付；而
        「发现错位」的形态是 argmin≠0，那条路径不受影响（上两条负例保底）。
        """
        cfg = _cfg()
        cfg.align_margin_min = 1e9          # 任何真实优势都到不了 → 强制走弱优势分支
        z, frames = build_clip(Path(tempfile.mkdtemp(dir=self.tmp)), n_depth=6)
        res = {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, cfg)}
        r = res[cd.DEPTH_RULE_ALIGN]
        self.assertEqual(r.status, SKIP, r.evidence)
        self.assertIn("测不了", r.evidence)

    def test_wrong_channel_name_fails_format(self):
        res = self._run(n_depth=2, channel="R")
        self.assertEqual(res[cd.DEPTH_RULE_FORMAT].status, FAIL)

    def test_float32_pixels_fail_format(self):
        res = self._run(n_depth=2, pixel_type=2)
        r = res[cd.DEPTH_RULE_FORMAT]
        self.assertEqual(r.status, FAIL)
        self.assertIn("FLOAT", r.evidence)

    def test_constant_velocity_is_unobservable_and_skips(self):
        """匀速直线：整体错一帧**几何上不可观测**——必须 SKIP 并说明，不能假 PASS。

        这不是规则的缺陷，是这类运动的固有性质：反投影只依赖相对位姿，
        而匀速直线下相对位姿不随整体偏移改变。
        """
        z, frames = build_clip(self.tmp, n_depth=6, dz=0.6, accel=0.0, yaw_rate=0.0)
        res = {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}
        r = res[cd.DEPTH_RULE_ALIGN]
        self.assertEqual(r.status, SKIP, r.evidence)
        self.assertIn("不可观测", r.evidence)

    def test_static_camera_skips_reprojection(self):
        """静止时反投影退化为恒等，证明不了任何事——必须 SKIP 而不是假 PASS。"""
        z, frames = build_clip(self.tmp, n_depth=3, dz=0.0, accel=0.0, yaw_rate=0.0)
        res = {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}
        self.assertEqual(res[cd.DEPTH_RULE_REPROJ].status, SKIP)
        self.assertIn("静止", res[cd.DEPTH_RULE_REPROJ].evidence)
        self.assertEqual(res[cd.DEPTH_RULE_ALIGN].status, SKIP)

    def test_unquantized_values_fail(self):
        """未按 cm 截断的深度（原始浮点）必须被量化组抓到。"""
        zpath = self.tmp / "depth.zip"
        frames = []
        with zipfile.ZipFile(zpath, "w") as zf:
            for i in range(2):
                px = DF.render_depth(W, H_, (0, 0, -20 + i * 0.6), DF.yaw_quat(0), FX, FY, CX, CY)
                px = [v + 0.003 if v == v else v for v in px]     # 破坏 cm 网格
                zf.writestr(f"depth_{i*2}.exr", DF.write_exr_bytes(W, H_, px))
                frames.append(_frame(i * 2, (0, 0, -20 + i * 0.6), DF.yaw_quat(0), f"depth_{i*2}"))
        res = {r.rule: r for r in cd.check_depth_content(zpath, frames, SYSINFO, self.cfg)}
        r = res[cd.DEPTH_RULE_QUANT]
        self.assertEqual(r.status, FAIL, r.evidence)

    def test_corrupt_zip_fails_not_crashes(self):
        bad = self.tmp / "depth.zip"
        bad.write_bytes(b"not a zip")
        self.assertEqual(cd.check_flat(bad).status, FAIL)
        res = {r.rule: r for r in cd.check_depth_content(bad, [], SYSINFO, self.cfg)}
        self.assertEqual(res[cd.DEPTH_RULE_FORMAT].status, FAIL)


class ExrReaderTest(unittest.TestCase):
    """读取器本身：压缩档往返 + 不支持形态要明确报错而不是给半通结论。"""

    def test_roundtrip_all_supported_compressions(self):
        px = [float("nan") if y < 2 else (x + y * 0.25) for y in range(20) for x in range(16)]
        for comp in (DF.COMP_NONE, DF.COMP_ZIPS, DF.COMP_ZIP):
            img = exr.read(DF.write_exr_bytes(16, 20, px, compression=comp))
            self.assertEqual((img.width, img.height), (16, 20))
            self.assertEqual(img.channel_names, ["Y"])
            got = img.floats()
            for i, v in enumerate(px):
                e = exr.HALF_TO_FLOAT[DF.f2h(v)]
                if e != e:
                    self.assertNotEqual(got[i], got[i], f"comp={comp} i={i}")
                else:
                    self.assertEqual(got[i], e, f"comp={comp} i={i}")

    def test_tiled_flag_rejected(self):
        raw = bytearray(DF.write_exr_bytes(8, 8, [1.0] * 64))
        raw[4] = 2; raw[5] = 2                      # version 低字节 2 + tiled 位
        with self.assertRaises(exr.ExrError) as e:
            exr.read(bytes(raw))
        self.assertIn("tiled", str(e.exception))

    def test_bad_magic_rejected(self):
        with self.assertRaises(exr.ExrError):
            exr.read(b"\x00\x01\x02\x03" + b"\x02\x00\x00\x00" + b"\x00" * 32)

    def test_unsupported_compression_rejected(self):
        raw = bytearray(DF.write_exr_bytes(8, 8, [1.0] * 64, compression=DF.COMP_NONE))
        i = raw.find(b"compression\x00compression\x00")
        raw[i + len("compression\x00compression\x00") + 4] = 4      # PIZ
        with self.assertRaises(exr.ExrError) as e:
            exr.read(bytes(raw))
        self.assertIn("PIZ", str(e.exception))


class GoldenCrossLanguageTest(unittest.TestCase):
    """跨语言契约：读 C# ExrWriter 真实写出的 EXR。

    夹具由 `engine-data/enginecapture/Runtime/Core/ExrWriter.cs` 在 Unity 里生成，
    内容是解析可算的固定图案。任一侧改动导致口径漂移，这条会红。
    """

    FIXTURE = Path(__file__).resolve().parents[2] / "fixtures" / "depth_csharp_zip_64x40.exr"

    def test_reads_csharp_written_exr(self):
        if not self.FIXTURE.is_file():
            self.skipTest(f"夹具缺失：{self.FIXTURE}")
        img = exr.read_file(self.FIXTURE)
        self.assertEqual((img.width, img.height), (64, 40))
        self.assertEqual(img.channel_names, ["Y"])
        self.assertEqual(img.channels[0][1], exr.PIXELTYPE_HALF)
        self.assertEqual(img.compression, 3, "ZIP")

        vals = img.floats()
        # 生成规则（与 C# 侧一致）：y<3 → NaN；否则 trunc((0.5 + x*0.37 + y*1.1)*100)/100
        for y in range(40):
            for x in range(64):
                got = vals[y * 64 + x]
                if y < 3:
                    self.assertNotEqual(got, got, f"({x},{y}) 应为 NaN")
                else:
                    want = exr.HALF_TO_FLOAT[DF.f2h(
                        math.trunc((0.5 + x * 0.37 + y * 1.1) * 100.0) / 100.0)]
                    self.assertEqual(got, want, f"({x},{y})")


if __name__ == "__main__":
    unittest.main()
