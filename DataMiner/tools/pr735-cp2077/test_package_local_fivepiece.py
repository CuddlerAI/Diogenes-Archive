import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

import zstandard


SCRIPT = Path(__file__).with_name("package-local-fivepiece.py")
SPEC = importlib.util.spec_from_file_location("cp2077_fivepiece", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class RawInputPackagingTests(unittest.TestCase):
    def source_segment(self, root: Path, events):
        segment = root / "segment"
        segment.mkdir()
        (segment / "video.mp4").write_bytes(b"test-video")
        frames_path = segment / "frames.jsonl.zst"
        with frames_path.open("wb") as target:
            with zstandard.ZstdCompressor(level=1).stream_writer(target) as writer:
                writer.write(
                    json.dumps({"session_frame": 0, "source_qpc": 1_000}).encode("utf-8")
                    + b"\n"
                )
        events_path = segment / "events.jsonl.zst"
        with events_path.open("wb") as target:
            with zstandard.ZstdCompressor(level=1).stream_writer(target) as writer:
                for event in events:
                    writer.write(json.dumps(event).encode("utf-8") + b"\n")
        events_sha256 = MODULE.sha256(events_path)
        video_sha256 = MODULE.sha256(segment / "video.mp4")
        frames_sha256 = MODULE.sha256(frames_path)
        meta = {
            "n_frames": 1,
            "video": {"path": "video.mp4", "sha256": video_sha256},
            "frames": {
                "path": "frames.jsonl.zst",
                "lines": 1,
                "sha256": frames_sha256,
            },
            "input_status": "raw_input_v1",
            "events": {
                "path": "events.jsonl.zst",
                "lines": len(events),
                "sha256": events_sha256,
            },
            "health": {
                "input_raw_reports": len(events),
                "input_overflow": 0,
                "input_boundary_reorder_drops": 0,
            },
        }
        (segment / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
        (segment / "sha256.json").write_text(
            json.dumps(
                {
                    "video.mp4": video_sha256,
                    "frames.jsonl.zst": frames_sha256,
                    "events.jsonl.zst": events_sha256,
                    "meta.json": MODULE.sha256(segment / "meta.json"),
                }
            ),
            encoding="utf-8",
        )
        return segment, meta

    def complete_events(self):
        return [
            {"seq": 1, "qpc": 100, "type": "key_down", "vk": 87},
            {"seq": 2, "qpc": 110, "type": "mouse_move", "dx": 2, "dy": -1},
            {"seq": 3, "qpc": 120, "type": "key_up", "vk": 87},
        ]

    def test_packages_auditable_keyboard_and_mouse_stream(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            segment, meta = self.source_segment(root, self.complete_events())
            staging = root / "staging"
            staging.mkdir()
            contract = MODULE.package_raw_input(
                segment, staging, meta, {"input_queue_overflows": 0}
            )
            self.assertEqual(contract["event_count"], 3)
            self.assertEqual(contract["keyboard_event_count"], 2)
            self.assertEqual(contract["mouse_move_event_count"], 1)
            self.assertTrue((staging / "raw-input/events.jsonl.zst").is_file())
            self.assertTrue((staging / "raw-input/contract.json").is_file())
            self.assertEqual(
                MODULE.sha256(staging / "raw-input/events.jsonl.zst"),
                contract["files"]["events"]["sha256"],
            )

    def test_rejects_missing_mouse_stream(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            segment, meta = self.source_segment(
                root,
                [
                    {"seq": 1, "qpc": 100, "type": "key_down", "vk": 87},
                    {"seq": 2, "qpc": 120, "type": "key_up", "vk": 87},
                ],
            )
            staging = root / "staging"
            staging.mkdir()
            with self.assertRaisesRegex(SystemExit, "no mouse movement"):
                MODULE.package_raw_input(
                    segment, staging, meta, {"input_queue_overflows": 0}
                )

    def test_rejects_event_hash_drift(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            segment, meta = self.source_segment(root, self.complete_events())
            meta["events"]["sha256"] = "sha256:" + "0" * 64
            staging = root / "staging"
            staging.mkdir()
            with self.assertRaisesRegex(SystemExit, "inconsistent"):
                MODULE.package_raw_input(
                    segment, staging, meta, {"input_queue_overflows": 0}
                )

    def test_source_segment_hashes_bind_video_and_frame_timing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            segment, meta = self.source_segment(root, self.complete_events())
            rows = MODULE.verify_source_segment(segment, meta)
            self.assertEqual(rows, [{"session_frame": 0, "source_qpc": 1_000}])
            (segment / "video.mp4").write_bytes(b"changed-video")
            with self.assertRaisesRegex(SystemExit, "differ from sha256"):
                MODULE.verify_source_segment(segment, meta)

    def test_nearest_observation_must_be_inside_explicit_qpc_window(self):
        selected, delta = MODULE.nearest_within([(100, "sample")], 105, 5, "camera")
        self.assertEqual(selected, (100, "sample"))
        self.assertEqual(delta, 5)
        with self.assertRaisesRegex(SystemExit, "too far"):
            MODULE.nearest_within([(100, "sample")], 106, 5, "camera")

    def test_raw_depth_path_must_be_a_direct_child(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            depth = root / "raw-depth"
            depth.mkdir()
            (depth / "000000000000.d32").write_bytes(b"depth")
            self.assertEqual(
                MODULE.direct_child(depth, "000000000000.d32", "depth").name,
                "000000000000.d32",
            )
            with self.assertRaisesRegex(SystemExit, "plain leaf"):
                MODULE.direct_child(depth, "../outside.d32", "depth")

    @staticmethod
    def watermark_row(session_frame, value):
        bits = [
            (MODULE.crc8_low16(value) >> shift) & 1 for shift in range(7, -1, -1)
        ] + [(value >> shift) & 1 for shift in range(15, -1, -1)]
        return {
            "session_frame": session_frame,
            "block_means": [200 if bit else 40 for bit in bits],
        }

    @staticmethod
    def v2_report(**fields):
        report = {
            "schema": "dm-cp2077-local-raw-rgb-depth-pairing/v2",
            "passed": True,
            "rgb_before_depth_pairs": 0,
            "unmatched_depth_present_count": 0,
            "low16_unambiguous_within_session": True,
            "blockers": [],
            "non_trainable": True,
            "recording_ready": False,
        }
        report.update(fields)
        return report

    def test_strong_pairing_is_recomputed_and_cannot_be_weakened(self):
        depth = [{"present_seq": 100, "present_qpc": 1_000}]
        frames = [{"session_frame": 0, "source_qpc": 1_000}]
        watermark = [self.watermark_row(0, 100)]
        pairing = self.v2_report(
            rgb_frame_count=1,
            decoded_rgb_watermarks=1,
            decoded_rgb_watermark_rate=1.0,
            depth_frame_count=1,
            exact_present_matches=1,
            exact_depth_rgb_coverage=1.0,
            measured_present_interval_ticks=0,
            pairing_window_lead_ticks=625,
            pairing_window_lag_ticks=625,
            min_compositor_lag_ticks=0,
            median_compositor_lag_ticks=0,
            max_compositor_lag_ticks=0,
        )
        selected = MODULE.validate_strong_pairing(
            pairing, {"n_frames": 1}, depth, watermark, frames, 10_000
        )
        self.assertEqual(selected, depth)
        pairing["max_compositor_lag_ticks"] = 1
        with self.assertRaisesRegex(SystemExit, "inconsistent"):
            MODULE.validate_strong_pairing(
                pairing, {"n_frames": 1}, depth, watermark, frames, 10_000
            )

    def test_strong_pairing_mirrors_client_v2_one_sided_lag_window(self):
        # The same fixture as the client's
        # rgb_depth_pairing_accepts_measured_compositor_lag_below_target_rate:
        # 13.38 Hz Presents with compositor lags up to 118.5 ms, past the old
        # symmetric 62.5 ms bound.
        frequency = 10_000_000
        interval = 747_300
        lags = [249_802, 433_345, 572_720, 678_446, 824_455, 1_185_343]
        depth = [
            {"present_seq": 400 + index, "present_qpc": 50_000_000 + interval * index}
            for index in range(len(lags))
        ]
        frames = [
            {"session_frame": index, "source_qpc": row["present_qpc"] + lag}
            for index, (row, lag) in enumerate(zip(depth, lags))
        ]
        watermark = [
            self.watermark_row(index, row["present_seq"]) for index, row in enumerate(depth)
        ]
        pairing = self.v2_report(
            rgb_frame_count=6,
            decoded_rgb_watermarks=6,
            decoded_rgb_watermark_rate=1.0,
            depth_frame_count=6,
            exact_present_matches=6,
            exact_depth_rgb_coverage=1.0,
            measured_present_interval_ticks=interval,
            pairing_window_lead_ticks=frequency // 16,
            pairing_window_lag_ticks=interval * 3,
            min_compositor_lag_ticks=249_802,
            median_compositor_lag_ticks=678_446,
            max_compositor_lag_ticks=1_185_343,
        )
        selected = MODULE.validate_strong_pairing(
            pairing, {"n_frames": 6}, depth, watermark, frames, frequency
        )
        self.assertEqual(selected, depth)
        stale = dict(pairing, schema="dm-cp2077-local-raw-rgb-depth-pairing/v1")
        with self.assertRaisesRegex(SystemExit, "inconsistent"):
            MODULE.validate_strong_pairing(
                stale, {"n_frames": 6}, depth, watermark, frames, frequency
            )

    def test_strong_pairing_keeps_the_causal_lead_bound(self):
        # RGB stamped more than 1/16 s before the game's Present cannot be the
        # same frame; a report claiming that pair is refused.
        frequency = 10_000_000
        interval = 747_300
        depth = [
            {"present_seq": 500, "present_qpc": 70_000_000},
            {"present_seq": 501, "present_qpc": 70_000_000 + interval},
        ]
        frames = [
            {"session_frame": 0, "source_qpc": depth[0]["present_qpc"] + 100},
            {"session_frame": 1, "source_qpc": depth[1]["present_qpc"] - frequency // 16 - 1},
        ]
        watermark = [self.watermark_row(0, 500), self.watermark_row(1, 501)]
        claimed = self.v2_report(
            rgb_frame_count=2,
            decoded_rgb_watermarks=2,
            decoded_rgb_watermark_rate=1.0,
            depth_frame_count=2,
            exact_present_matches=2,
            exact_depth_rgb_coverage=1.0,
            measured_present_interval_ticks=interval,
            pairing_window_lead_ticks=frequency // 16,
            pairing_window_lag_ticks=interval * 3,
            min_compositor_lag_ticks=100,
            median_compositor_lag_ticks=100,
            max_compositor_lag_ticks=100,
        )
        with self.assertRaisesRegex(SystemExit, "inconsistent"):
            MODULE.validate_strong_pairing(
                claimed, {"n_frames": 2}, depth, watermark, frames, frequency
            )
        honest = dict(
            claimed,
            exact_present_matches=1,
            exact_depth_rgb_coverage=0.5,
            unmatched_depth_present_count=1,
        )
        with self.assertRaisesRegex(SystemExit, "thresholds are not satisfied"):
            MODULE.validate_strong_pairing(
                honest, {"n_frames": 2}, depth, watermark, frames, frequency
            )


if __name__ == "__main__":
    unittest.main()
