"""depth 采集可交付性判定。反例取自 `depth-formal-20260804-195844` 的真实形状。"""

import json
import shutil
import tempfile
import unittest
from pathlib import Path

import helpers as H  # noqa: E402

from engine_delivery import audit_depth  # noqa: E402
from engine_delivery.audit_depth import audit_index, audit_run  # noqa: E402


def _status(rep, rule):
    return next((s for r, s, _ in rep.checks if r == rule), None)


def _evidence(rep, rule):
    return next((e for r, s, e in rep.checks if r == rule), "")


FULL_EXTRA = {"near": 0.1, "far": 10000.0, "reverse_z": True,
              "metric_scale": 1.0, "qpc_ns": 1, "video_frame": 0}


class DepthAuditTest(unittest.TestCase):
    def test_complete_run_is_deliverable(self):
        rows = H.depth_rows(range(0, 200, 2), extra=FULL_EXTRA)
        rep = audit_index("run", rows, pose_index=[{"present_seq": s}
                                                   for s in range(0, 200, 2)])
        self.assertEqual(rep.verdict, "OK", rep.to_dict())

    def test_in_bucket_run_shape_is_blocked(self):
        """在库那份的形状：只有 present_seq/宽高/格式/路径，没有投影、没有时间基、
        没有位姿旁车，还带 15 处帧洞。四条阻断一条不少。"""
        seqs = [s for s in range(2, 400, 2) if not (100 <= s <= 132)]  # 挖掉 17 帧
        rep = audit_index("depth-formal-20260804-195844", H.depth_rows(seqs))
        self.assertEqual(rep.verdict, "BAD")
        self.assertEqual(_status(rep, "depth.resolution"), "OK")
        self.assertEqual(_status(rep, "depth.parity"), "OK")
        for rule in ("depth.continuity", "depth.linearizable",
                     "depth.timebase", "depth.pose_sidecar"):
            self.assertEqual(_status(rep, rule), "BAD", rule)
        self.assertIn("缺 17 帧", _evidence(rep, "depth.continuity"))

    def test_gap_report_names_longest_run(self):
        seqs = [2, 4, 6, 20, 22, 40]
        rep = audit_index("run", H.depth_rows(seqs, extra=FULL_EXTRA),
                          pose_index=[{"present_seq": s} for s in seqs])
        ev = _evidence(rep, "depth.continuity")
        self.assertIn("2 处断口", ev)
        self.assertIn("共缺 14 帧", ev)   # 6→20 缺 6、22→40 缺 8
        self.assertIn("22→40", ev)

    def test_projection_alone_is_not_enough(self):
        rows = H.depth_rows(range(0, 20, 2), extra={"near": 0.1, "qpc_ns": 1})
        rep = audit_index("run", rows, pose_index=[{"present_seq": s}
                                                  for s in range(0, 20, 2)])
        self.assertEqual(_status(rep, "depth.linearizable"), "BAD")
        self.assertIn("metric_scale", _evidence(rep, "depth.linearizable"))

    def test_wrong_resolution(self):
        rows = H.depth_rows(range(0, 10, 2), extra=FULL_EXTRA)
        for r in rows:
            r["width"], r["height"] = 1920, 1080
        rep = audit_index("run", rows, pose_index=[{"present_seq": s}
                                                  for s in range(0, 10, 2)])
        self.assertEqual(_status(rep, "depth.resolution"), "BAD")

    def test_odd_present_seq(self):
        rows = H.depth_rows([0, 1, 2, 4], extra=FULL_EXTRA)
        rep = audit_index("run", rows,
                          pose_index=[{"present_seq": s} for s in (0, 1, 2, 4)])
        self.assertEqual(_status(rep, "depth.parity"), "BAD")

    def test_pose_hole_is_blocked(self):
        seqs = list(range(0, 20, 2))
        rep = audit_index("run", H.depth_rows(seqs, extra=FULL_EXTRA),
                          pose_index=[{"present_seq": s} for s in seqs if s != 8])
        self.assertEqual(_status(rep, "depth.pose_sidecar"), "BAD")
        self.assertIn("首个 8", _evidence(rep, "depth.pose_sidecar"))

    def test_reads_run_dir(self):
        tmp = Path(tempfile.mkdtemp(prefix="dlv_depth_"))
        try:
            seqs = list(range(0, 20, 2))
            (tmp / "depth-index.jsonl").write_text(
                "\n".join(json.dumps(r) for r in H.depth_rows(seqs, extra=FULL_EXTRA)),
                encoding="utf-8")
            (tmp / "pose-index.jsonl").write_text(
                "\n".join(json.dumps({"present_seq": s}) for s in seqs), encoding="utf-8")
            rep = audit_run(tmp)
            self.assertEqual(rep.verdict, "OK", rep.to_dict())
            self.assertEqual(rep.rows, len(seqs))
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    def test_missing_index(self):
        tmp = Path(tempfile.mkdtemp(prefix="dlv_depth_"))
        try:
            self.assertEqual(audit_run(tmp).verdict, "BAD")
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()


class DuplicateAndOddStepTests(unittest.TestCase):
    """`_gaps` 看不见的两种坏形状。

    重复让相邻差变成 0（永远不 `> 2`），奇数步长让「缺了几帧」算出 0——
    两者都会让一份坏索引拿到 OK。
    """

    @staticmethod
    def _row(seq):
        return {"present_seq": seq, "source_frame": seq, "width": 1280, "height": 720,
                "path": f"raw/{seq}.f32", "proj_a": 1.0, "proj_b": 0.1,
                "metric_scale": 1.0}

    def test_duplicate_present_seq_is_blocking(self):
        rows = [self._row(2 * i) for i in range(20)]
        rows.append({**self._row(14), "path": "raw/dup.f32"})   # 两份 payload 争 depth_14
        poses = [{"present_seq": 2 * i} for i in range(20)]
        rep = audit_depth.audit_index("dup", rows, poses)
        self.assertEqual(rep.verdict, audit_depth.BAD)
        hit = [e for r, s, e in rep.checks if r == "depth.unique" and s == audit_depth.BAD]
        self.assertTrue(hit and "14" in hit[0], rep.checks)

    def test_clean_index_reports_unique(self):
        rows = [self._row(2 * i) for i in range(20)]
        poses = [{"present_seq": 2 * i} for i in range(20)]
        rep = audit_depth.audit_index("clean", rows, poses)
        self.assertEqual(rep.verdict, audit_depth.OK, rep.blocking)

    def test_odd_step_does_not_inflate_gap_count(self):
        # 58→61 步长 3：parity 该报，断口不该记一条「缺 0 帧」
        self.assertEqual(audit_depth._gaps([58, 61]), [])
        self.assertEqual(audit_depth._gaps([58, 94]), [(58, 94, 17)])

    def test_row_without_present_seq_is_blocking(self):
        rows = [self._row(2 * i) for i in range(5)]
        rows.append({"width": 1280, "height": 720, "path": "raw/x.f32"})
        rep = audit_depth.audit_index("noseq", rows)
        self.assertEqual(rep.verdict, audit_depth.BAD)
        self.assertTrue(any(r == "depth.index" and s == audit_depth.BAD
                            for r, s, _ in rep.checks), rep.checks)
