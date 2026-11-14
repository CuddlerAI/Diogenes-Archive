import argparse
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from gta_capture import cli
from gta_capture.errors import ContractError, GtaCaptureError
from gta_capture.jsonio import sha256_file, write_json_atomic
from gta_capture.stage_export import (PICTURE_SETTINGS, POINTER, REQUIRED_RENDER_SETTINGS, check_game_settings, encode_lane, encode_quad, find_pointer,
                                      load_lane_result, normalize_output_root, pointer_frames, render_profile,
                                      take_export_inputs)

try:
    import numpy as np
except ImportError:   # the pointer search runs where the frames are, on the capture machine
    np = None
try:
    from PIL import Image
except ImportError:   # and reads them with Pillow there
    Image = None


def render_ini(root: Path, **overrides) -> Path:
    settings = dict(REQUIRED_RENDER_SETTINGS, RenderOutputFolder="D:\\workspace\\mc4\\renders\\")
    settings.update(overrides)
    lines = ["[Render]"] + [f"{k}={v}" for k, v in settings.items() if v is not None]
    path = root / "Render.ini"
    path.write_bytes(("\r\n".join(lines) + "\r\n").encode("utf-8"))
    return path


class RenderProfileTests(unittest.TestCase):
    def test_the_profile_is_the_file_itself_and_its_normalized_root(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = render_ini(Path(tmp))
            profile = render_profile(path)
            self.assertEqual(profile.sha256, sha256_file(path))
            self.assertEqual(profile.output_root, "D:\\workspace\\mc4\\renders")

    def test_every_setting_the_bridge_checks_must_hold(self):
        for key, bad in (("RenderFps", "60"), ("RenderMode", "Video"), ("RenderSettleFrames", "2"),
                         ("RenderSamples", None), ("RenderCaptureMode", "Walking"), ("RenderShutter", "0.5")):
            with tempfile.TemporaryDirectory() as tmp, self.assertRaises(ContractError, msg=key):
                render_profile(render_ini(Path(tmp), **{key: bad}))

    def test_an_inline_comment_is_not_part_of_the_value(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = render_ini(Path(tmp), RenderFps="30   ; output rate")
            self.assertEqual(render_profile(path).output_root, "D:\\workspace\\mc4\\renders")

    def test_the_output_root_must_be_one_absolute_drive_path(self):
        self.assertEqual(normalize_output_root("D:/a/b/"), "D:\\a\\b")
        self.assertEqual(normalize_output_root("C:\\"), "C:\\")
        for bad in ("", "renders", "\\\\server\\share", "D:a"):
            with self.assertRaises(ContractError, msg=bad):
                normalize_output_root(bad)


class GameSettingsTests(unittest.TestCase):
    def _settings(self, root, body):
        path = Path(root) / "settings.xml"
        path.write_text('<?xml version="1.0" encoding="UTF-8"?>\n<Settings>\n  <graphics>\n' + body + "  </graphics>\n</Settings>\n",
                        encoding="utf-8")
        return path

    def test_post_fx_up_to_high_passes_and_says_what_it_read(self):
        with tempfile.TemporaryDirectory() as tmp:
            for value in (0, 1):
                path = self._settings(tmp, f'    <PostFX value="{value}" />\n    <DoF value="true" />\n')
                self.assertEqual(check_game_settings(path),
                                 {"path": str(path.resolve()), "sha256": sha256_file(path), "post_fx": value,
                                  "picture": {tag: None for tag in PICTURE_SETTINGS}})

    def test_the_picture_settings_an_export_renders_under_are_recorded(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._settings(tmp, '    <PostFX value="1" />\n    <MSAA value="4" />\n    <TXAA_Enabled value="true" />\n'
                                       '    <SamplingMode value="8" />\n    <MaxLodScale value="1.000000" />\n')
            picture = check_game_settings(path)["picture"]
            self.assertEqual((picture["MSAA"], picture["TXAA_Enabled"], picture["SamplingMode"], picture["MaxLodScale"]),
                             ("4", "true", "8", "1.000000"))
            self.assertIsNone(picture["ShadowQuality"])   # not in this file: said so, not guessed

    def test_post_fx_above_high_is_refused_whatever_the_dof_switch_says(self):
        with tempfile.TemporaryDirectory() as tmp:
            for value, name in ((2, "Very High"), (3, "Ultra")):
                for dof in ("true", "false"):
                    path = self._settings(tmp, f'    <PostFX value="{value}" />\n    <DoF value="{dof}" />\n')
                    with self.assertRaisesRegex(ContractError, f"Post FX is {name}"):
                        check_game_settings(path)

    def test_a_file_that_does_not_say_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ContractError, "not found"):
                check_game_settings(Path(tmp) / "settings.xml")
            for body in ("", '    <PostFX />\n', '    <PostFX value="high" />\n',
                         '    <PostFX value="1" />\n    <PostFX value="2" />\n'):
                with self.assertRaisesRegex(ContractError, "one PostFX value"):
                    check_game_settings(self._settings(tmp, body))
            path = Path(tmp) / "settings.xml"
            path.write_text("<Settings><graphics>", encoding="utf-8")
            with self.assertRaisesRegex(ContractError, "cannot read"):
                check_game_settings(path)


class TakeExportInputTests(unittest.TestCase):
    def _take(self, root: Path) -> tuple[Path, dict]:
        take = root / "takes" / "AB"
        (take / "source").mkdir(parents=True)
        (take / "config").mkdir(parents=True)
        (take / "source" / "master-replay-1.clip").write_bytes(b"clip")
        (take / "source" / "master-player-telemetry.jsonl").write_text("{}\n", encoding="utf-8")
        (take / "config" / "camera-tape.json").write_text('{"n_frames": 30}', encoding="utf-8")
        (take / "config" / "camera-tape.bin").write_bytes(b"tape")
        result = {"clips": [{"index": 1, "sha256": sha256_file(take / "source" / "master-replay-1.clip"),
                             "source_clip_name": "2026-9-23-clip-0037.clip", "duration_s": 1.01}],
                  "player_telemetry_sha256": sha256_file(take / "source" / "master-player-telemetry.jsonl")}
        return take, result

    def test_a_single_clip_take_hands_over_its_sealed_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            take, result = self._take(Path(tmp))
            inputs = take_export_inputs(take, result)
            self.assertEqual(inputs.source_clip_name, "2026-9-23-clip-0037.clip")
            self.assertEqual(inputs.replay, take / "source" / "master-replay-1.clip")
            self.assertEqual(inputs.more_clips, ())

    def test_a_take_the_recorder_cut_up_hands_over_its_clips_in_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            take, result = self._take(Path(tmp))
            second = take / "source" / "master-replay-2.clip"
            second.write_bytes(b"clip two")
            result["clips"].append({"index": 2, "sha256": sha256_file(second),
                                    "source_clip_name": "2026-9-23-clip-0038.clip"})
            inputs = take_export_inputs(take, result)
            self.assertEqual(inputs.source_clip_name, "2026-9-23-clip-0037.clip")
            self.assertEqual([(c.path, c.source_clip_name) for c in inputs.more_clips],
                             [(second, "2026-9-23-clip-0038.clip")])

    def test_changed_or_missing_files_and_split_takes_are_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            take, result = self._take(Path(tmp))
            with self.assertRaises(ContractError):   # one clip listed twice
                take_export_inputs(take, {**result, "clips": result["clips"] * 2})
            with self.assertRaises(ContractError):   # a second clip that was never sealed
                take_export_inputs(take, {**result, "clips": result["clips"] + [
                    {"index": 2, "sha256": "0" * 64, "source_clip_name": "2026-9-23-clip-0038.clip"}]})
            with self.assertRaises(ContractError):   # clips that end before the tape's last frame
                take_export_inputs(take, {**result, "clips": [dict(result["clips"][0], duration_s=0.96)]})
            (take / "source" / "master-replay-1.clip").write_bytes(b"other")
            with self.assertRaises(ContractError):
                take_export_inputs(take, result)
        with tempfile.TemporaryDirectory() as tmp:
            take, result = self._take(Path(tmp))
            (take / "config" / "camera-tape.bin").unlink()
            with self.assertRaises(ContractError):
                take_export_inputs(take, result)


@unittest.skipUnless(np is not None, "numpy is not installed")
class PointerTests(unittest.TestCase):
    def _frame(self):
        # sand-like: neither as dark as the outline nor as bright as the fill
        return np.random.default_rng(7).integers(90, 180, size=(120, 200, 3), dtype=np.uint8)

    def _draw_pointer(self, rgb, left, top):
        for y, row in enumerate(POINTER):
            for x, ch in enumerate(row):
                if ch in "#o":
                    rgb[top + y, left + x] = 0 if ch == "#" else 255
        return rgb

    def test_the_pointer_is_found_where_it_is_drawn(self):
        self.assertEqual(find_pointer(self._draw_pointer(self._frame(), 150, 60)), (150, 60))

    @unittest.skipUnless(Image is not None, "Pillow is not installed")
    def test_every_frame_file_that_shows_the_pointer_is_listed(self):
        with tempfile.TemporaryDirectory() as tmp:
            pattern = str(Path(tmp) / "frame_%06d.png")
            for index in range(3):
                rgb = self._frame()
                if index == 1:
                    self._draw_pointer(rgb, 150, 60)
                Image.fromarray(rgb).save(pattern.replace("%06d", f"{index:06d}"))
            self.assertEqual(pointer_frames(pattern, 3, workers=2), [1])

    def test_a_clean_frame_or_a_white_box_with_dark_edges_is_not_the_pointer(self):
        self.assertIsNone(find_pointer(self._frame()))
        rgb = self._frame()
        rgb[20:60, 40:80] = 255
        rgb[18:60, 38:40] = 0   # a dark left edge, like the pointer's
        rgb[18:20, 38:80] = 0   # and a dark top
        self.assertIsNone(find_pointer(rgb))


class LaneResultTests(unittest.TestCase):
    def test_only_a_lane_whose_frames_were_searched_clean_is_encoded(self):
        with tempfile.TemporaryDirectory() as tmp:
            take = Path(tmp)
            lane = {"frame_pattern": "frame_%06d.png", "frame_count": 1830}
            for pixels in (None, {"pointer_frames": 1774}):
                write_json_atomic(take / "evidence" / "native-lane-1-result.json",
                                  {"ok": True, "lane": lane, "pixel_check": pixels})
                with self.assertRaises(ContractError):
                    load_lane_result(take, 1)
            write_json_atomic(take / "evidence" / "native-lane-1-result.json",
                              {"ok": True, "lane": lane, "pixel_check": {"pointer_frames": 0}})
            self.assertEqual(load_lane_result(take, 1), lane)


class FakePipe:
    """The controller's pipe, answering from a script and recording the requests."""
    def __init__(self, answers):
        self.answers = answers
        self.sent = []

    def __call__(self, _pipe):
        return self

    def request(self, command, payload=None):
        self.sent.append((command, payload))
        return self.answers[command]

    def close(self):
        pass


class LaneQueueCliTests(unittest.TestCase):
    def _args(self, root, **kw):
        base = dict(group=str(root), take="AB", lane=None, lanes=None, render_ini="Render.ini",
                    game_settings="settings.xml", arm_only=False, redo=False, pipe="pipe")
        base.update(kw)
        return argparse.Namespace(**base)

    def test_the_queue_is_distinct_lanes_with_lane_four_last(self):
        self.assertEqual(cli._stage_export_lanes(self._args(".", lanes="1,2,3,4")), [1, 2, 3, 4])
        self.assertEqual(cli._stage_export_lanes(self._args(".", lanes="3,1")), [3, 1])
        self.assertEqual(cli._stage_export_lanes(self._args(".", lane=2)), [2])
        for bad in ("1,1", "4,1", "0,1", "5", "1,,2", "a"):
            with self.assertRaises(GtaCaptureError, msg=bad):
                cli._stage_export_lanes(self._args(".", lanes=bad))
        with self.assertRaises(GtaCaptureError):   # both, or neither
            cli._stage_export_lanes(self._args(".", lane=1, lanes="1"))
        with self.assertRaises(GtaCaptureError):
            cli._stage_export_lanes(self._args("."))

    def _run(self, root, pipe, settings=None, **kw):
        profile = SimpleNamespace(path=Path("Render.ini"), sha256="p" * 64, output_root="D:\\renders")
        settings = settings or mock.Mock(return_value={"path": "settings.xml", "sha256": "s" * 64, "post_fx": 1})
        inputs = SimpleNamespace(tape_bin=root / "t.bin", tape_json=root / "t.json", replay=root / "r.clip",
                                 replay_sha256="r" * 64, source_clip_name="c.clip",
                                 player_telemetry=root / "p.jsonl", player_telemetry_sha256="q" * 64,
                                 more_clips=())
        (root / "t.json").write_text("{}", encoding="utf-8")
        pixels = {"checked_frames": 3, "pointer_frames": 0, "first_pointer_frame": None,
                  "last_pointer_frame": None}
        with mock.patch.object(cli, "JsonLinePipeClient", pipe), \
                mock.patch.object(cli, "take_export_inputs", return_value=inputs), \
                mock.patch.object(cli, "take_result", return_value={}), \
                mock.patch.object(cli, "render_profile", return_value=profile), \
                mock.patch.object(cli, "check_game_settings", settings), \
                mock.patch.object(cli, "check_lane_pixels", return_value=pixels), \
                mock.patch.object(cli, "_print"):
            return cli._cmd_stage_take_export(self._args(root, **kw))

    def _answers(self, **extra):
        answers = {"hello": {"story_mode": True, "online_session": False},
                   "probe_state": {"controller_generation": 1, "record_binding_ready": True,
                                   "master_replay_sha256": "r" * 64, "master_clip_count": 1}}
        answers.update(extra)
        return answers

    def _lane(self, n):
        return {"lane": n, "frame_pattern": "frame_%06d.png", "frame_count": 3,
                "render_profile_sha256": "p" * 64, "normalized_output_root": "D:\\renders"}

    def test_nothing_is_armed_under_game_settings_that_blur_the_export(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pipe = FakePipe(self._answers(arm_export_lanes={"armed": True, "lanes": [1, 2, 3, 4]}))
            refuse = mock.Mock(side_effect=ContractError("the game's Post FX is Very High"))
            with self.assertRaisesRegex(ContractError, "Post FX"):
                self._run(root, pipe, settings=refuse, lanes="1,2,3,4", arm_only=True)
            self.assertEqual(pipe.sent, [])   # refused before the game heard of it
            # collecting a queue exports nothing: the settings are not asked for
            collect = FakePipe(self._answers(collect_export_lanes={"lanes": [self._lane(n) for n in (1, 2, 3, 4)]}))
            self._run(root, collect, settings=refuse, lanes="1,2,3,4")
            refuse.assert_called_once()

    def test_arming_sends_the_queue_and_keeps_its_answer(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pipe = FakePipe(self._answers(arm_export_lanes={"armed": True, "lanes": [1, 2, 3, 4]}))
            self.assertEqual(self._run(root, pipe, lanes="1,2,3,4", arm_only=True), 0)
            command, payload = pipe.sent[-1]
            self.assertEqual((command, payload["lanes"]), ("arm_export_lanes", "1,2,3,4"))
            armed = json.loads((root / "takes" / "AB" / "evidence" / "native-lanes-arm.json").read_text())
            self.assertEqual(armed["lane"]["lanes"], [1, 2, 3, 4])

    def test_collecting_writes_each_sealed_lane_and_fails_on_a_lane_that_did_not_seal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pipe = FakePipe(self._answers(collect_export_lanes={"lanes": [
                self._lane(1), self._lane(2), {"lane": 3, "error": "the clock stopped"},
                {"lane": 4, "error": "not exported: lane 3 did not seal"}]}))
            self.assertEqual(self._run(root, pipe, lanes="1,2,3,4"), 1)
            self.assertNotIn("bind_take", [c for c, _ in pipe.sent])
            evidence = root / "takes" / "AB" / "evidence"
            self.assertEqual(sorted(p.name for p in evidence.glob("native-lane-*-result.json")),
                             ["native-lane-1-result.json", "native-lane-2-result.json"])
            self.assertTrue(json.loads((evidence / "native-lane-2-result.json").read_text())["ok"])

    def test_a_queue_answering_for_other_lanes_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pipe = FakePipe(self._answers(collect_export_lanes={"lanes": [self._lane(1)]}))
            with self.assertRaises(GtaCaptureError):
                self._run(root, pipe, lanes="1,2")

    def test_a_lane_with_a_result_is_not_armed_again_without_redo(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            evidence = root / "takes" / "AB" / "evidence"
            evidence.mkdir(parents=True)
            (evidence / "native-lane-2-result.json").write_text("{}", encoding="utf-8")
            pipe = FakePipe(self._answers(arm_export_lanes={"armed": True}))
            with self.assertRaises(GtaCaptureError):
                self._run(root, pipe, lanes="1,2", arm_only=True)
            self.assertEqual(self._run(root, pipe, lanes="1,2", arm_only=True, redo=True), 0)
            self.assertFalse((evidence / "native-lane-2-result.json").exists())


@unittest.skipUnless(shutil.which("ffmpeg") and shutil.which("ffprobe"), "ffmpeg is not installed")
class EncodeTests(unittest.TestCase):
    def test_every_frame_is_kept_when_the_sound_ends_early(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            pattern = str(root / "frame_%06d.png")
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x36:rate=30",
                            "-frames:v", "10", "-start_number", "0", pattern], check=True)
            audio = root / "audio.wav"   # 9 frames' worth: -shortest used to drop the tenth
            subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "sine=duration=0.3", str(audio)],
                           check=True)
            out = encode_lane({"frame_pattern": pattern, "frame_count": 10}, root / "lane.mp4", audio=audio)
            probe = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                                    "stream=nb_frames", "-of", "csv=p=0", str(out)],
                                   check=True, capture_output=True, text=True)
            self.assertEqual(probe.stdout.strip(), "10")

    def test_the_quad_preview_makes_its_own_folder(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            videos = []
            for i in range(4):
                v = root / f"video_{i + 1}.mp4"
                subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x36:rate=30",
                                "-frames:v", "5", "-pix_fmt", "yuv420p", str(v)], check=True)
                videos.append(v)
            out = encode_quad(videos, root / "preview" / "four-camera-preview.mp4")
            self.assertTrue(out.is_file())


if __name__ == "__main__":
    unittest.main()
