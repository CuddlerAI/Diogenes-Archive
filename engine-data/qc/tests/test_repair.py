"""flip-depth 存量修复工具测试（docs/88）。

要害是**位精确**：修复只许改行序，不许改任何一个像素的位模式——
修完 depth.quantize / depth.range 必须原样通过，否则「修复」本身就是二次污染。
夹具用 depth_fixture 的独立写出器造输入（不用被测代码的逆运算验它自己）。
"""

import math
import shutil
import tempfile
import unittest
import zipfile
from pathlib import Path

import depth_fixture as DF
from test_depth import SYSINFO, W, H_, _cfg, build_clip

from engine_qc import checks_depth as cd
from engine_qc import cli, exr
from engine_qc.repair import RepairError, flip_depth_zip, flip_exr_rows


def _px(w=16, h=20):
    """带 NaN 天空 + 逐行变化的合成图，行间互不相同，翻错任何一行都会被发现。"""
    return [float("nan") if y < 3 else math.trunc((1.0 + y * 0.37 + x * 0.01) * 100) / 100
            for y in range(h) for x in range(w)]


class FlipExrTest(unittest.TestCase):
    def test_rows_reversed_bit_exact(self):
        for comp in (DF.COMP_NONE, DF.COMP_ZIPS, DF.COMP_ZIP):
            src = DF.write_exr_bytes(16, 20, _px(), compression=comp)
            a, b = exr.read(src), exr.read(flip_exr_rows(src))
            self.assertEqual(b.compression, a.compression)
            for y in range(20):
                self.assertEqual(b.bits["Y"][y * 16:(y + 1) * 16],
                                 a.bits["Y"][(19 - y) * 16:(20 - y) * 16],
                                 f"压缩 {comp} 第 {y} 行位模式不匹配")

    def test_double_flip_is_identity_on_pixels(self):
        src = DF.write_exr_bytes(16, 20, _px())
        twice = exr.read(flip_exr_rows(flip_exr_rows(src)))
        self.assertEqual(twice.bits["Y"], exr.read(src).bits["Y"])

    def test_float_channel_supported(self):
        src = DF.write_exr_bytes(16, 20, _px(), pixel_type=2)
        a, b = exr.read(src), exr.read(flip_exr_rows(src))
        for y in range(20):
            got = b.bits["Y"][y * 16:(y + 1) * 16].tobytes()
            want = a.bits["Y"][(19 - y) * 16:(20 - y) * 16].tobytes()
            self.assertEqual(got, want)

    def test_uint_channel_rejected(self):
        src = DF.write_exr_bytes(16, 20, _px(), pixel_type=0)
        with self.assertRaises(RepairError):
            flip_exr_rows(src)


class FlipZipTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="qc_repair_"))
        self.cfg = _cfg()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_zip_shape_preserved_and_repaired_clip_passes_qc(self):
        """端到端：翻转坏 clip → flip-depth → 内容组全部重新裁决为绿。"""
        z, frames = build_clip(self.tmp, n_depth=5, vflip=True)
        before = {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}
        self.assertEqual(before[cd.DEPTH_RULE_ORIENT].status, "FAIL")

        fixed = self.tmp / "depth.fixed.zip"
        n = flip_depth_zip(z, fixed)
        self.assertEqual(n, 5)

        with zipfile.ZipFile(z) as a, zipfile.ZipFile(fixed) as b:
            ia, ib = a.infolist(), b.infolist()
            self.assertEqual([i.filename for i in ia], [i.filename for i in ib])
            self.assertEqual([i.compress_type for i in ia], [i.compress_type for i in ib])

        after = {r.rule: r for r in cd.check_depth_content(fixed, frames, SYSINFO, self.cfg)}
        for rule in (cd.DEPTH_RULE_FORMAT, cd.DEPTH_RULE_QUANT, cd.DEPTH_RULE_NAN,
                     cd.DEPTH_RULE_RANGE, cd.DEPTH_RULE_ORIENT):
            self.assertEqual(after[rule].status, "PASS",
                             f"{rule}: {after[rule].evidence}")
        # 反投影对镜像本就无感（盲区），但修复后同样不得变红
        self.assertNotEqual(after[cd.DEPTH_RULE_REPROJ].status, "FAIL")

    def test_cli_in_place_keeps_bak_and_refuses_second_run(self):
        z, frames = build_clip(self.tmp, n_depth=3, vflip=True)
        self.assertEqual(cli.main(["flip-depth", str(z), "--in-place"]), 0)
        bak = z.with_name(z.name + ".bak")
        self.assertTrue(bak.exists(), "原件必须保留为 .bak")
        after = {r.rule: r for r in cd.check_depth_content(z, frames, SYSINFO, self.cfg)}
        self.assertEqual(after[cd.DEPTH_RULE_ORIENT].status, "PASS",
                         after[cd.DEPTH_RULE_ORIENT].evidence)
        # 备份还在时拒绝再跑——双翻等于把修好的又翻坏
        self.assertEqual(cli.main(["flip-depth", str(z), "--in-place"]), 1)
