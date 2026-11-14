"""打包与形态审计。反例取自 R2 上那条真数据的形状（docs/87 §2）。"""

import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: E402

from engine_delivery.audit_mc import audit_dir, audit_keys  # noqa: E402
from engine_delivery.pack_mc import (gameinfo_skeleton, ledger_row,  # noqa: E402
                                     pack_clip)
from engine_delivery.spec import (MC_REQUIRED_FILES, mc_group_name,  # noqa: E402
                                  mc_relpath)


class SpecTest(unittest.TestCase):
    def test_twelve_required_files(self):
        self.assertEqual(len(MC_REQUIRED_FILES), 12)
        self.assertIn("video_1.mp4", MC_REQUIRED_FILES)
        self.assertIn("action_camera_4.json", MC_REQUIRED_FILES)
        self.assertIn("systeminfo_3.json", MC_REQUIRED_FILES)

    def test_group_name(self):
        self.assertEqual(mc_group_name(2026, 3, 11, "xingqiuchongqi", 1),
                         "2026_03_11_xingqiuchongqi_01")
        with self.assertRaises(ValueError):
            mc_group_name(2026, 3, 11, "Xing_Qiu", 1)

    def test_relpath_derives_mmdd_from_group(self):
        """MMDD 由组名推出，不另给——两处分别填写是个现成的错误来源。"""
        self.assertEqual(mc_relpath("MegacityMetro", "2026_08_04_megacity_01"),
                         "2MultiCamera/MegacityMetro/0804/2026_08_04_megacity_01")
        self.assertEqual(
            mc_relpath("MegacityMetro", "2026_08_04_megacity_01", prefix="internal-validation"),
            "internal-validation/2MultiCamera/MegacityMetro/0804/2026_08_04_megacity_01")


class PackTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="dlv_pack_"))
        self.src = H.make_capture_dir(self.tmp / "capture")
        self.out = self.tmp / "out"

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def test_copies_exactly_twelve(self):
        res = pack_clip(self.src, self.out, game="MegacityMetro",
                        group="2026_08_04_megacity_01", qc_verdict="PASS")
        self.assertTrue(res.ok, res.problems)
        dest = Path(res.dest)
        self.assertEqual(sorted(p.name for p in dest.iterdir()),
                         sorted(MC_REQUIRED_FILES))
        self.assertEqual(sorted(res.skipped_internal),
                         ["audio_mix.wav", "covis.json", "dyn_metrics.json",
                          "plan.json", "provenance.json"])

    def test_refuses_non_pass_qc(self):
        res = pack_clip(self.src, self.out, game="MegacityMetro",
                        group="2026_08_04_megacity_01", qc_verdict="FAIL")
        self.assertFalse(res.ok)
        self.assertFalse((self.out).exists())

    def test_refuses_unverified_qc(self):
        """UNVERIFIED 也不放行——「没验过」不是「验过了」。"""
        res = pack_clip(self.src, self.out, game="MegacityMetro",
                        group="2026_08_04_megacity_01", qc_verdict="UNVERIFIED")
        self.assertFalse(res.ok)

    def test_missing_required_file_blocks(self):
        src = H.make_capture_dir(self.tmp / "cap2", drop=("video_3.mp4",))
        res = pack_clip(src, self.out, game="MegacityMetro",
                        group="2026_08_04_megacity_01", qc_verdict="PASS")
        self.assertFalse(res.ok)
        self.assertIn("缺必要文件 video_3.mp4", res.problems)

    def test_packed_tree_passes_audit(self):
        pack_clip(self.src, self.out, game="MegacityMetro",
                  group="2026_08_04_megacity_01", qc_verdict="PASS")
        rep = audit_dir(self.out)
        self.assertEqual(rep.verdict, "OK", rep.to_dict())

    def test_ledger_and_gameinfo(self):
        res = pack_clip(self.src, self.out, game="MegacityMetro",
                        group="2026_08_04_megacity_01", qc_verdict="PASS")
        row = ledger_row(res, "MegacityMetro", "open_plaza")
        self.assertEqual(row["文件数"], 12)
        self.assertEqual(row["状态"], "OK")
        gi = gameinfo_skeleton("MegacityMetro", "1920×1080")
        self.assertEqual(gi["游戏名字"], "MegacityMetro")
        self.assertEqual(gi["gameprocessName"], "MegacityMetro")
        self.assertEqual(gi["键鼠映射规则"], "")  # 骨架只保证不漏项，不替人填
        # 模板 A–S 共 19 列（docs/69 §10.3）；K–Q 七列是 2026-08-04 拿到读权限后
        # 才看到的，骨架漏了它们等于回到只抄截图的老状态
        self.assertEqual(len(gi), 19)
        for col in ("空间属性", "世界时间设定", "敌人类型", "画风维度", "可提供滤镜变种"):
            self.assertIn(col, gi)


class AuditKeysTest(unittest.TestCase):
    BASE = "internal-validation/2MultiCamera/MegacityMetro/0804/2026_08_04_megacity_01"

    def _keys(self, names):
        return [f"{self.BASE}/{n}" for n in names]

    def test_clean_group_ok(self):
        rep = audit_keys("r2://x", self._keys(MC_REQUIRED_FILES))
        self.assertEqual(rep.verdict, "OK", rep.to_dict())

    def test_in_bucket_shape_is_rejected(self):
        """R2 上那条的真实形状：12 必要文件 + 4 个我方内部件。"""
        names = MC_REQUIRED_FILES + ["plan.json", "covis.json",
                                     "dyn_metrics.json", "provenance.json"]
        rep = audit_keys("r2://diogenes-engine-delivery", self._keys(names))
        self.assertEqual(rep.verdict, "BAD")
        problem = rep.groups[0].problems[0]
        for n in ("plan.json", "covis.json", "dyn_metrics.json", "provenance.json"):
            self.assertIn(n, problem)

    def test_missing_file_is_reported(self):
        rep = audit_keys("r2://x", self._keys(MC_REQUIRED_FILES[:-1]))
        self.assertEqual(rep.verdict, "BAD")
        self.assertIn("缺必要文件 1/12", rep.groups[0].problems[0])

    def test_date_dir_mismatch(self):
        keys = [f"2MultiCamera/G/0803/2026_08_04_megacity_01/{n}"
                for n in MC_REQUIRED_FILES]
        rep = audit_keys("r2://x", keys)
        self.assertEqual(rep.verdict, "BAD")
        self.assertTrue(any("不一致" in p for p in rep.groups[0].problems))

    def test_bad_group_name(self):
        keys = [f"2MultiCamera/G/0804/clip-004000/{n}" for n in MC_REQUIRED_FILES]
        rep = audit_keys("r2://x", keys)
        self.assertTrue(any("不合文法" in p for p in rep.groups[0].problems))

    def test_wrong_depth_is_orphan(self):
        """多一层或少一层目录都算 orphan——入库路径对不上是整批返工级问题。"""
        keys = ["2MultiCamera/G/0804/2026_08_04_megacity_01/sub/video_1.mp4"]
        rep = audit_keys("r2://x", keys)
        self.assertEqual(rep.orphans, keys)
        self.assertEqual(rep.verdict, "BAD")

    def test_gameprocessname_mismatch(self):
        gk = "2MultiCamera/Wrong/0804/2026_08_04_megacity_01"
        keys = [f"{gk}/{n}" for n in MC_REQUIRED_FILES]
        rep = audit_keys("r2://x", keys, sysinfo={gk: H.SYSINFO})
        self.assertTrue(any("gameProcessName" in p for p in rep.groups[0].problems))

    def test_duplicate_group_name(self):
        keys = ([f"2MultiCamera/A/0804/2026_08_04_megacity_01/{n}" for n in MC_REQUIRED_FILES]
                + [f"2MultiCamera/B/0804/2026_08_04_megacity_01/{n}" for n in MC_REQUIRED_FILES])
        rep = audit_keys("r2://x", keys)
        self.assertTrue(any("重复" in p for g in rep.groups for p in g.problems))


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
