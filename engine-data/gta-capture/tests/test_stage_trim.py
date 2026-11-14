"""Cutting a take's hold: the video's frame 0 is stage tick 0, the sound cut with it."""

import contextlib
import io
import math
import shutil
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from gta_capture import cli
from gta_capture.errors import ContractError
from gta_capture.jsonio import write_json_atomic
from gta_capture.stage_export import encode_lane


def _decode(path: Path, *args: str) -> bytes:
    return subprocess.run(["ffmpeg", "-v", "error", "-i", str(path), *args, "-"], check=True,
                          capture_output=True).stdout


def _gray(path: Path) -> float:
    raw = _decode(path, "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray")
    return sum(raw) / len(raw)


def _sound(path: Path) -> list[int]:
    raw = _decode(path, "-map", "0:a", "-f", "s16le", "-ac", "1", "-ar", "48000")
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def _rms(samples: list[int]) -> float:
    return math.sqrt(sum(s * s for s in samples) / len(samples))


class SkipTests(unittest.TestCase):
    def test_a_skip_outside_the_lane_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            for i in range(3):
                (Path(tmp) / f"frame_{i:06d}.png").write_bytes(b"")
            lane = {"frame_pattern": str(Path(tmp) / "frame_%06d.png"), "frame_count": 3}
            for skip in (-1, 3, True, 1.0):
                with self.assertRaises(ContractError, msg=repr(skip)):
                    encode_lane(lane, Path(tmp) / "lane.mp4", skip=skip)

    @unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "ffmpeg is not installed")
    def test_the_video_starts_at_the_skipped_frame_and_the_sound_with_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pattern = str(root / "frame_%06d.png")
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "color=c=black:s=64x36:r=30",
                            "-vf", "geq=lum='N*20':cb=128:cr=128",   # frame i is a flat gray of 20 i
                            "-frames:v", "10", "-start_number", "0", pattern], check=True)
            audio = root / "audio.wav"   # silent for the first 0.1 s -- the three skipped frames -- then a tone
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i",
                            r"aevalsrc=if(lt(t\,0.1)\,0\,0.5*sin(2*PI*440*t)):s=48000:d=0.4", str(audio)], check=True)
            lane = {"frame_pattern": pattern, "frame_count": 10}
            whole = encode_lane(lane, root / "whole.mp4", audio=audio)
            cut = encode_lane(lane, root / "cut.mp4", audio=audio, skip=3)
            probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                                    "stream=nb_frames", "-of", "csv=p=0", str(cut)],
                                   check=True, capture_output=True, text=True)
            self.assertEqual(probe.stdout.strip(), "7")
            sources = [_gray(Path(pattern.replace("%06d", f"{i:06d}"))) for i in range(10)]
            first = _gray(cut)
            self.assertEqual(min(range(10), key=lambda i: abs(sources[i] - first)), 3)   # its frame 0 is frame 3
            whole_sound, cut_sound = _sound(whole), _sound(cut)
            # the seven frames' length -- up to one AAC frame (1024 samples) more: some ffmpeg
            # builds keep the encoder's padding of the last frame (win-pc's 8.1.2 decodes 11264)
            self.assertGreaterEqual(len(cut_sound), 48000 * 7 // 30)
            self.assertLess(len(cut_sound), 48000 * 7 // 30 + 1024)
            self.assertLess(_rms(whole_sound[:960]), 100)              # uncut: the first 20 ms are the silence
            self.assertGreater(_rms(cut_sound[:960]), 8000)            # cut: the tone from the first sample on


class EncodeCommandTests(unittest.TestCase):
    def test_stage_take_encode_cuts_the_takes_hold(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp) / "takes" / "C"
            write_json_atomic(take / "config" / "camera-tape.json", {"n_frames": 1830, "source": {"hold_frames": 30}})
            with mock.patch.object(cli, "load_lane_result", return_value={"frame_count": 1830}), \
                    mock.patch.object(cli, "encode_lane", return_value=take / "capture" / "video_2.mp4") as encode, \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(cli.main(["stage-take-encode", "--group", tmp, "--take", "C", "--lane", "2"]), 0)
            self.assertEqual(encode.call_args.kwargs["skip"], 30)


if __name__ == "__main__":
    unittest.main()
