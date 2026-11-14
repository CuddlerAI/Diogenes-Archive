"""Small filesystem fixtures test scratch lifetime, never real capture pixels."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from gta_capture.errors import BackendError, ContractError
from gta_capture.jsonio import sha256_file
from gta_capture.media import materialize_videos


class MediaScratchTests(unittest.TestCase):
    def _inputs(self, root):
        artifacts, events, originals = {}, {}, {}
        for lane in range(1, 5):
            folder = root / f"raw-lane-{lane}"
            folder.mkdir()
            events[lane] = []
            for frame in range(2):
                path = folder / f"frame_{frame:06d}.png"
                path.write_bytes(f"unit-test-RGB-{lane}-{frame}".encode())
                originals[path] = path.read_bytes()
                events[lane].append({"output_path": str(path.resolve()),
                                     "output_bytes": path.stat().st_size,
                                     "output_sha256": sha256_file(path)})
            artifacts[lane] = {"frame_pattern": str(folder / "frame_%06d.png"),
                               "frame_count": 2}
        audio = root / "original-audio.wav"
        audio.write_bytes(b"unit-test-sealed-audio")
        originals[audio] = audio.read_bytes()
        return artifacts, events, {"audio_path": str(audio), "audio_bytes": audio.stat().st_size,
                                   "audio_sha256": sha256_file(audio)}, originals

    @staticmethod
    def _probe():
        duration = str(2 / 30)
        return {"streams": [
            {"codec_type": "video", "nb_read_frames": "2", "avg_frame_rate": "30/1",
             "r_frame_rate": "30/1", "duration": duration},
            {"codec_type": "audio", "sample_rate": "48000", "channels": 2,
             "duration": duration},
        ], "format": {"duration": duration}}

    def _materialize(self, root, inputs):
        artifacts, events, audio, _ = inputs
        return materialize_videos(root / "capture", artifacts, audio, ffmpeg="ffmpeg",
                                  ffprobe="ffprobe", lane_events=events)

    def _assert_originals(self, originals):
        for path, before in originals.items():
            self.assertEqual(path.read_bytes(), before, str(path))

    def test_each_completed_lane_releases_only_its_copies_before_next_encode(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            inputs = self._inputs(root)
            encoded = []

            def run(command):
                output = Path(command[-1])
                if "-an" in command:
                    lane = int(output.name.split("-")[1])
                    for previous in range(1, lane):
                        self.assertFalse((root / "media-scratch" / f"lane-{previous}-frames").exists())
                        self.assertFalse((root / "media-scratch" / f"lane-{previous}-silent.mp4").exists())
                        self.assertTrue((root / "capture" / f"video_{previous}.mp4").is_file())
                    present = list((root / "media-scratch").glob("lane-*-frames"))
                    self.assertEqual(present, [root / "media-scratch" / f"lane-{lane}-frames"])
                    encoded.append(lane)
                output.write_bytes(b"unit-test-encoded-media")

            with patch("gta_capture.media._run", side_effect=run) as encoder, \
                    patch("gta_capture.media._probe_media", return_value=self._probe()) as probe:
                outputs = self._materialize(root, inputs)
            self.assertEqual(encoded, [1, 2, 3, 4])
            self.assertEqual(encoder.call_count, 8)
            self.assertEqual(probe.call_count, 4)
            self.assertEqual(len(outputs), 4)
            self.assertTrue(all(Path(path).is_file() for path in outputs))
            self.assertFalse((root / "media-scratch").exists())
            self._assert_originals(inputs[3])

    def test_later_encode_mux_or_probe_failure_preserves_raw_completed_video_and_current_scratch(self):
        for failure in ("encode", "mux", "probe"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                inputs = self._inputs(root)

                def run(command):
                    output = Path(command[-1])
                    output.write_bytes(b"unit-test-partial-or-complete-media")
                    if ((failure == "encode" and output.name == "lane-2-silent.mp4")
                            or (failure == "mux" and output.name == "video_2.mp4.muxing.mp4")):
                        raise BackendError("unit-test media command failure")

                def probe(_tool, path):
                    report = self._probe()
                    if failure == "probe" and path.name == "video_2.mp4.muxing.mp4":
                        report["streams"][0]["nb_read_frames"] = "1"
                    return report

                with patch("gta_capture.media._run", side_effect=run), \
                        patch("gta_capture.media._probe_media", side_effect=probe):
                    with self.assertRaises((BackendError, ContractError)):
                        self._materialize(root, inputs)
                self.assertTrue((root / "capture/video_1.mp4").is_file())
                self.assertFalse((root / "capture/video_2.mp4").exists())
                scratch = root / "media-scratch"
                self.assertFalse((scratch / "lane-1-frames").exists())
                self.assertFalse((scratch / "lane-1-silent.mp4").exists())
                self.assertEqual(len(list((scratch / "lane-2-frames").glob("*.png"))), 2)
                self.assertTrue((scratch / "lane-2-silent.mp4").is_file())
                self.assertTrue((scratch / "master-audio.wav").is_file())
                self.assertFalse((scratch / "lane-3-frames").exists())
                self._assert_originals(inputs[3])

    def test_existing_failed_scratch_is_refused_untouched_before_any_media_command(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            inputs = self._inputs(root)
            old = root / "media-scratch/lane-3-frames/old-failure.png"
            old.parent.mkdir(parents=True)
            old.write_bytes(b"preserve prior failure")
            with patch("gta_capture.media._run") as run:
                with self.assertRaisesRegex(ContractError, "already exists"):
                    self._materialize(root, inputs)
                run.assert_not_called()
            self.assertEqual(old.read_bytes(), b"preserve prior failure")
            self.assertEqual([p for p in (root / "media-scratch").rglob("*") if p.is_file()], [old])
            self._assert_originals(inputs[3])

    def test_existing_completed_video_or_mux_temp_is_never_overwritten(self):
        for name in ("video_3.mp4", "video_2.mp4.muxing.mp4"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                inputs = self._inputs(root)
                old = root / "capture" / name
                old.parent.mkdir()
                old.write_bytes(b"preserve earlier media")
                with patch("gta_capture.media._run") as run:
                    with self.assertRaisesRegex(ContractError, "output already exists"):
                        self._materialize(root, inputs)
                    run.assert_not_called()
                self.assertEqual(old.read_bytes(), b"preserve earlier media")
                self.assertFalse((root / "media-scratch").exists())
                self._assert_originals(inputs[3])

    def test_output_appearing_during_encoding_is_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            inputs = self._inputs(root)
            appeared = root / "capture/video_1.mp4"

            def run(command):
                Path(command[-1]).write_bytes(b"unit-test-encoded-media")
                appeared.parent.mkdir()
                appeared.write_bytes(b"preserve concurrently published output")

            with patch("gta_capture.media._run", side_effect=run), \
                    patch("gta_capture.media.mux_master_audio") as mux:
                with self.assertRaisesRegex(ContractError, "appeared or was redirected"):
                    self._materialize(root, inputs)
                mux.assert_not_called()
            self.assertEqual(appeared.read_bytes(), b"preserve concurrently published output")
            self.assertTrue((root / "media-scratch/lane-1-silent.mp4").is_file())
            self._assert_originals(inputs[3])

    def test_encoder_returning_original_path_is_rejected_without_deleting_original(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            inputs = self._inputs(root)
            raw = next(iter(inputs[3]))
            with patch("gta_capture.media.encode_frame_sequence", return_value=raw), \
                    patch("gta_capture.media.mux_master_audio") as mux:
                with self.assertRaisesRegex(ContractError, "escaped or changed ownership"):
                    self._materialize(root, inputs)
                mux.assert_not_called()
            self.assertEqual(len(list((root / "media-scratch/lane-1-frames").glob("*.png"))), 2)
            self._assert_originals(inputs[3])

    def test_unowned_file_blocks_cleanup_and_remains_intact(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            inputs = self._inputs(root)
            surprise = root / "media-scratch/lane-1-frames/not-created-by-stager.txt"

            def run(command):
                Path(command[-1]).write_bytes(b"unit-test-media")

            def probe(_tool, _path):
                surprise.write_bytes(b"preserve unowned file")
                return self._probe()

            with patch("gta_capture.media._run", side_effect=run), \
                    patch("gta_capture.media._probe_media", side_effect=probe):
                with self.assertRaisesRegex(ContractError, "unowned files"):
                    self._materialize(root, inputs)
            self.assertEqual(surprise.read_bytes(), b"preserve unowned file")
            self.assertEqual(len(list(surprise.parent.glob("*.png"))), 2)
            self.assertTrue((root / "media-scratch/lane-1-silent.mp4").is_file())
            self.assertTrue((root / "capture/video_1.mp4").is_file())
            self._assert_originals(inputs[3])


if __name__ == "__main__":
    unittest.main()
