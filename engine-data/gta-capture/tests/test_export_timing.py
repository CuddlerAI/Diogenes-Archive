"""tools/export_timing.py: a lane's frame times from its PNGs' modification times, the renders a frame took from
the Editor heartbeat in its frame events, and the GPU log over the lane's window (2026-10-13)."""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import export_timing  # noqa: E402


def lane(root: Path, lane_no: int, n: int, t0: float, dt: float, hb_step: int, stall_at: int | None = None) -> Path:
    take = root / "takes" / "C"
    renders = root / "renders" / f"render_{lane_no:04d}"
    renders.mkdir(parents=True, exist_ok=True)
    (take / "evidence").mkdir(parents=True, exist_ok=True)
    t = t0
    events = []
    for i in range(n):
        p = renders / f"frame_{i:06d}.png"
        p.write_bytes(b"x")
        t += dt + (2.0 if stall_at == i else 0.0)
        os.utime(p, (t, t))
        events.append({"output_frame": i, "editor_binding": {"identity_heartbeat": 100 + hb_step * i}})
    (take / "evidence" / f"frame-events-lane{lane_no}.jsonl").write_text(
        "\n".join(json.dumps(e) for e in events) + "\n", encoding="utf-8")
    (take / "evidence" / f"native-lane-{lane_no}-result.json").write_text(json.dumps(
        {"ok": True, "lane": {"frame_pattern": str(renders / "frame_%06d.png"), "frame_count": n}}), encoding="utf-8")
    return take


class ExportTimingTest(unittest.TestCase):
    def test_frame_times_renders_and_stalls(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            take = lane(root, 1, 31, 1_800_000_000.0, 0.09, 6, stall_at=10)
            r = export_timing.lane_timing(take, 1)
            self.assertEqual(r["frames_on_disk"], 31)
            self.assertEqual(r["stalls"], 1)
            self.assertAlmostEqual(r["frame_ms_p50"], 90.0, delta=1.0)
            self.assertEqual(r["renders_per_frame_p50"], 6)
            self.assertAlmostEqual(r["ms_per_render"], 15.0, delta=0.5)   # ~16.7: held to 60 Hz
            self.assertAlmostEqual(r["export_s"], 30 * 0.09 + 2.0, delta=0.05)
            self.assertEqual([lane_no for _, lane_no in export_timing.lanes_of(root)], [1])

    def test_gpu_log_over_the_lane(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            t0 = 1_800_000_000.0
            take = lane(root, 2, 11, t0, 1.0, 5)
            from datetime import datetime
            rows = ["timestamp, utilization.gpu [%], memory.used [MiB], memory.total [MiB], temperature.gpu, "
                    "clocks.current.graphics [MHz], power.draw [W], clocks_throttle_reasons.active"]
            for k in range(-3, 15):
                ts = datetime.fromtimestamp(t0 + k).strftime("%Y/%m/%d %H:%M:%S.000")
                rows.append(f"{ts}, {90 + (k % 3)} %, {7900 + k} MiB, 8188 MiB, {70 + k}, 2400 MHz, 110.0 W, "
                            + ("0x0000000000000020" if k == 5 else "0x0000000000000000"))
            log = root / "gpu.csv"
            log.write_text("\n".join(rows) + "\n", encoding="utf-8")
            r = export_timing.lane_timing(take, 2, export_timing.gpu_rows(log))
            g = r["gpu"]
            self.assertEqual(g["mem_total_mib"], 8188)
            self.assertGreaterEqual(g["mem_max_mib"], 7910)
            self.assertEqual(g["throttle_reasons"], ["0x0000000000000020"])
            self.assertEqual(r["renders_per_frame_p50"], 5)


if __name__ == "__main__":
    unittest.main()
