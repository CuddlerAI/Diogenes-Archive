import argparse
import importlib.util
import json
import os
import random
import shutil
import tempfile
import unittest
from types import SimpleNamespace
from unittest import mock
from pathlib import Path


MODULE_PATH = Path(__file__).resolve().parents[1] / "megacity_capture.py"
SPEC = importlib.util.spec_from_file_location("megacity_capture", MODULE_PATH)
MC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MC)


class ResolveSceneFileTests(unittest.TestCase):
    def args(self, path, world_offset=None):
        return argparse.Namespace(
            scene_file=str(path),
            world_offset=world_offset or MC.DEFAULT_WORLD_OFFSET,
        )

    def write_scene(self, root, payload):
        path = Path(root) / "scene.json"
        path.write_text(json.dumps(payload), encoding="utf-8")
        return path

    def test_reads_three_component_world_offset(self):
        with tempfile.TemporaryDirectory() as root:
            args = self.args(self.write_scene(root, {"world_offset": [1.23456, 2, -3]}))
            MC.resolve_scene_file(args)
            self.assertEqual(args.world_offset, "1.235,2.0,-3.0")

    def test_rejects_wrong_length_world_offset(self):
        with tempfile.TemporaryDirectory() as root:
            args = self.args(self.write_scene(root, {"world_offset": [1, 2]}))
            with self.assertRaisesRegex(SystemExit, "2"):
                MC.resolve_scene_file(args)

    def test_rejects_non_finite_world_offset(self):
        with tempfile.TemporaryDirectory() as root:
            args = self.args(self.write_scene(root, {"world_offset": [1, "NaN", 3]}))
            with self.assertRaisesRegex(SystemExit, "2"):
                MC.resolve_scene_file(args)

    def test_explicit_cli_offset_wins(self):
        with tempfile.TemporaryDirectory() as root:
            args = self.args(
                self.write_scene(root, {"world_offset": [1, 2, 3]}),
                world_offset="9,8,7",
            )
            MC.resolve_scene_file(args)
            self.assertEqual(args.world_offset, "9,8,7")


class LedgerRoundTripTests(unittest.TestCase):
    """账本持久化：pick_aspect 依赖的 clips/aspect_hits 必须原样恢复。"""

    def test_counts_survive_save_load(self):
        from trajgen.planner import QuotaTracker
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "ledger.json"
            q = QuotaTracker()
            q.note_clip("16:9")
            q.note_clip("9:16")
            q.note_track(["_no_such_cls_"], 24)
            q.save(path)
            r = QuotaTracker.load(path)
        self.assertEqual(r.clips, 2)
        self.assertEqual(r.aspect_hits["16:9"], 1)
        self.assertEqual(r.aspect_hits["9:16"], 1)
        self.assertEqual(r.tracks, 1)
        self.assertEqual(r.focal_hits[24], 1)  # 键必须还原成 int（JSON 只有 str 键）

    def test_missing_ledger_is_fresh_tracker(self):
        from trajgen.planner import QuotaTracker
        q = QuotaTracker.load(Path("no-such-dir") / "ledger.json")
        self.assertEqual(q.clips, 0)
        self.assertEqual(q.pick_aspect(random.Random(1)), "16:9")

    def test_restored_ledger_rotates_aspects(self):
        # 恢复后 clips>0 ⇒ 16:9 缺口为负、权重塌到 0.02，画幅开始轮转——
        # 这正是「每次新建 QuotaTracker ⇒ 44/44 全 16:9」的反命题
        from trajgen.planner import QuotaTracker
        q = QuotaTracker()
        q.note_clip("16:9")
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "ledger.json"
            q.save(path)
            r = QuotaTracker.load(path)
        picks = {r.pick_aspect(random.Random(s)) for s in range(40)}
        self.assertTrue(picks - {"16:9"}, "画幅未轮转：账本恢复没生效")


def _fake_proc(rc, stdout=b"", stderr=b""):
    return SimpleNamespace(returncode=rc, stdout=stdout, stderr=stderr)


class DirectorPlanStepTests(unittest.TestCase):
    def args(self, **kw):
        base = dict(mode="director", scene_file="scene.json", duration=60.0,
                    extras=2, covis_hz=2.0)
        base.update(kw)
        return argparse.Namespace(**base)

    def test_plan_fail_marks_and_returns_false(self):
        with tempfile.TemporaryDirectory() as root:
            runs = Path(root)
            clip = runs / "clip-000013"
            clip.mkdir()
            with mock.patch.object(MC.subprocess, "run",
                                   return_value=_fake_proc(1, b"direct-clip fail")):
                ok = MC.step_plan_director(clip, 13, self.args(), runs)
            self.assertFalse(ok)
            self.assertTrue(MC.has_marker(clip, "PLAN_FAIL"))
            self.assertFalse(MC.has_marker(clip, "PLANNED"))

    def test_plan_ok_marks_planned_with_aspect(self):
        with tempfile.TemporaryDirectory() as root:
            runs = Path(root)
            clip = runs / "clip-000013"
            plan = clip / "plan"
            plan.mkdir(parents=True)
            (plan / "director_report.json").write_text(
                json.dumps({"aspect": "9:16"}), encoding="utf-8")
            with mock.patch.object(MC.subprocess, "run",
                                   return_value=_fake_proc(0, b"ok")) as run:
                ok = MC.step_plan_director(clip, 13, self.args(), runs)
            self.assertTrue(ok)
            cmd = run.call_args[0][0]
            self.assertIn("--ledger", cmd)
            self.assertEqual(cmd[cmd.index("--ledger") + 1],
                             str(runs / "ledger.json"))
            self.assertEqual(MC.planned_aspect(clip), "9:16")

    def test_planned_marker_skips_replan(self):
        with tempfile.TemporaryDirectory() as root:
            runs = Path(root)
            clip = runs / "clip-000013"
            clip.mkdir()
            MC.set_marker(clip, "PLANNED", "1.0s aspect=16:9 mode=director")
            with mock.patch.object(MC.subprocess, "run") as run:
                ok = MC.step_plan_director(clip, 13, self.args(), runs)
            self.assertTrue(ok)
            run.assert_not_called()  # 不重跑 ⇒ 账本不会重复记账


class DirectorQcStepTests(unittest.TestCase):
    def test_director_qc_no_init_exemption(self):
        """cam4 改同路径异速率后初帧与前三路同簇，豁免已撤销 ⇒ 不许再传该旗标。"""
        rep = json.dumps({"results": [], "verdict": "PASS"}).encode()
        with tempfile.TemporaryDirectory() as root:
            clip = Path(root) / "clip-000013"
            clip.mkdir()
            with mock.patch.object(MC.subprocess, "run",
                                   return_value=_fake_proc(0, rep)) as run:
                ok, rule = MC.step_qc(clip, 13, SimpleNamespace(mode="director"))
            self.assertTrue(ok)
            self.assertNotIn("--init-exempt-lanes", run.call_args[0][0])

    def test_qc_unverified_is_not_a_pass(self):
        """规则没验成（如 ffprobe 缺失整组 UNVERIFIED）不得记为 QC_PASS。"""
        rep = json.dumps({"results": [
            {"rule": "video.fps", "status": "UNVERIFIED"},
            {"rule": "video.codec", "status": "UNVERIFIED"},
            {"rule": "plan.pink_coverage", "status": "PASS"},
        ]}).encode()
        with tempfile.TemporaryDirectory() as root:
            clip = Path(root) / "clip-000013"
            clip.mkdir()
            with mock.patch.object(MC.subprocess, "run",
                                   return_value=_fake_proc(0, rep)):
                ok, rule = MC.step_qc(clip, 13, SimpleNamespace(mode="director"))
            self.assertFalse(ok)
            self.assertEqual(rule, "qc.unverified")

    def test_qc_passes_ffmpeg_dir_on_path(self):
        """QC 子进程的 PATH 必须带上 ffmpeg 目录，否则 video 组整组 UNVERIFIED。"""
        rep = json.dumps({"results": [], "verdict": "PASS"}).encode()
        with tempfile.TemporaryDirectory() as root:
            clip = Path(root) / "clip-000013"
            clip.mkdir()
            ff = Path(root) / "bin" / ("ffmpeg.exe" if MC._WIN else "ffmpeg")
            ff.parent.mkdir()
            ff.write_bytes(b"")
            with mock.patch.dict(os.environ, {"DIOGENES_FFMPEG": str(ff)}):
                with mock.patch.object(MC.subprocess, "run",
                                       return_value=_fake_proc(0, rep)) as run:
                    MC.step_qc(clip, 13, SimpleNamespace(mode="director"))
            self.assertIn(str(ff.parent), run.call_args[1]["env"]["PATH"])


class AssetSyncTests(unittest.TestCase):
    def test_sync_refreshes_mtime_and_keeps_meta(self):
        """同步必须刷新 mtime（否则 Unity 复用旧程序集），且不动伺候中的 .meta。"""
        with tempfile.TemporaryDirectory() as root:
            src = Path(root) / "src" / "Runtime"
            src.mkdir(parents=True)
            (src / "A.cs").write_text("new", encoding="utf-8")
            proj = Path(root) / "proj"
            dstdir = proj / "Assets" / "DiogenesCapture" / "Runtime"
            dstdir.mkdir(parents=True)
            (dstdir / "A.cs").write_text("old", encoding="utf-8")
            os.utime(dstdir / "A.cs", (1000, 1000))       # 远古时间戳
            (dstdir / "A.cs.meta").write_text("guid", encoding="utf-8")
            (dstdir / "Gone.cs").write_text("stale", encoding="utf-8")
            with mock.patch.object(MC, "ENGINE_DATA", Path(root) / "ed"), \
                 mock.patch.object(MC, "UNITY_PROJECT", str(proj)):
                (Path(root) / "ed" / "enginecapture" / "Samples~").mkdir(parents=True)
                shutil.copytree(src.parent,
                                Path(root) / "ed" / "enginecapture"
                                / "Samples~" / "Megacity")
                MC.sync_capture_assets()
            self.assertEqual((dstdir / "A.cs").read_text(encoding="utf-8"), "new")
            self.assertGreater((dstdir / "A.cs").stat().st_mtime, 1e9)  # 已刷新
            self.assertTrue((dstdir / "A.cs.meta").is_file())           # meta 留着
            self.assertFalse((dstdir / "Gone.cs").is_file())            # 陈旧清除

    def test_classic_qc_has_no_exempt_flag(self):
        rep = json.dumps({"results": []}).encode()
        with tempfile.TemporaryDirectory() as root:
            clip = Path(root) / "clip-000013"
            clip.mkdir()
            with mock.patch.object(MC.subprocess, "run",
                                   return_value=_fake_proc(0, rep)) as run:
                ok, _ = MC.step_qc(clip, 13, SimpleNamespace(mode="classic"))
            self.assertTrue(ok)
            self.assertNotIn("--init-exempt-lanes", run.call_args[0][0])


if __name__ == "__main__":
    unittest.main()
