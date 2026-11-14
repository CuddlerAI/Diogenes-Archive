"""Exporting a four-worldline take: one lane at a time through RE+, then video.

A take's lanes render from its own replay in a later GTA process than the one
that recorded it, so the export binds the take by content (`bind_take`): the
sealed copy, the clip still in Rockstar's folder and the take's result must
hash alike, and the player grid is read back from the telemetry the take
sealed. Everything after the binding is the existing lane export -- plan v2,
the per-frame pixel/pose fence, the Editor identity checks -- unchanged.

The render profile is taken from the installed RE+ Render.ini itself: its
SHA-256 and its normalized output root, which RE+ re-hashes and compares on
every frame. This internal path seals no build receipt (the formal preflight
chain does), so what it produces is marked internal and not deliverable.

The per-frame fence proves each frame shows the planned camera; it does not see
what the game draws over it. Touching the mouse mid-export brings the Editor's
own pointer back, and it then stays on every later frame (AB lane 1 of
beach-demo-001 carried one from frame 56 on), so every collected lane is also
searched for that pointer.
"""

from __future__ import annotations

import json
import ntpath
import re
import shutil
import subprocess
import xml.etree.ElementTree as ET
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

from .errors import ContractError
from .jsonio import load_json, sha256_file

# What the patched RE+ bridge requires of Render.ini before it renders a frame
# (capture/diogenesbridge.cpp configValid). Takes render SLIDING on the engine's
# exact fixed step (patches/replus-v19-sliding.patch): the clip advances one
# frame per released present instead of a seek and a settle per frame (20 game
# frames each, 10 minutes a lane at 60 fps), every frame still checked against
# the walking seek time. The formal chain (scripts/render-profile.ps1) walks.
REQUIRED_RENDER_SETTINGS = {
    "EnableRenderer": "1", "RenderCaptureMode": "Sliding", "RenderShutter": "1", "RenderDepthOfField": "0",
    "RenderFps": "30", "RenderSamples": "1", "RenderHighlightBoost": "0.9", "RenderSettleFrames": "3",
    "RenderSettleSubFrames": "1", "RenderJpeg": "0", "RenderHideHud": "1", "ExportCloseWhenDone": "1",
    "RenderMode": "Frames", "RenderKeepFrames": "1", "RenderAudio": "1", "RenderMarkerSpeed": "1",
}


@dataclass(frozen=True)
class RenderProfile:
    path: Path
    sha256: str
    output_root: str   # normalized the way RE+ normalizes it


def normalize_output_root(value: str) -> str:
    """RE+'s normalization: an absolute drive path, backslashes, no trailing one."""
    if not value or "\n" in value or "\r" in value:
        raise ContractError("RenderOutputFolder must be one absolute Windows path")
    full = ntpath.normpath(value.replace("/", "\\"))
    if not re.match(r"^[A-Za-z]:\\", full):
        raise ContractError(f"RenderOutputFolder must be an absolute drive path, not {value!r}")
    while len(full) > 3 and full.endswith("\\"):
        full = full[:-1]
    return full


def render_profile(path) -> RenderProfile:
    """The installed Render.ini: every setting the bridge requires, exactly once,
    in its [Render] section, and the output root it names."""
    path = Path(path)
    if not path.is_file():
        raise ContractError(f"RE+ Render.ini not found: {path}")
    raw = path.read_bytes().decode("utf-8-sig")
    section, values = None, {}
    for line in raw.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped[1:-1].strip()
            continue
        if section != "Render" or "=" not in stripped or stripped.startswith(";"):
            continue
        key, value = stripped.split("=", 1)
        key = key.strip()
        value = value.split(";", 1)[0].strip()   # RE+ cuts inline comments too
        if key in values:
            raise ContractError(f"RE+ Render.ini sets {key} twice")
        values[key] = value
    for key, want in REQUIRED_RENDER_SETTINGS.items():
        if values.get(key) != want:
            raise ContractError(f"RE+ Render.ini must set {key}={want} (found {values.get(key)!r})")
    return RenderProfile(path.resolve(), sha256_file(path), normalize_output_root(values.get("RenderOutputFolder", "")))


# The game's own graphics settings (settings.xml, written by Settings > Graphics) a lane is
# exported under. Post FX at Very High or Ultra gives the picture a depth of field, and an
# export keeps the focus of the camera the take was RECORDED with, not the lane's: people a
# metre from the first-person lane were out of focus while the skyline was sharp, and the
# focus jumped with whatever the recording camera had looked at (the customer, 2026-09-29,
# beach-demo-003). Measured on that take, the same frames exported three times:
#   Post FX Very High, In-Game Depth Of Field Effects on    near person's sharpness 14.8
#   Post FX Very High, In-Game Depth Of Field Effects off   14.8 (the switch does nothing here)
#   Post FX High                                            92.5, colours and brightness as before
# RE+'s own depth of field is another thing (Render.ini RenderDepthOfField=0, above), and
# the Editor's per-marker one cannot be edited on these clips (its menu is greyed out).
MAX_POST_FX = 1
POST_FX_NAMES = ("Normal", "High", "Very High", "Ultra")
# settings.xml tags that decide how clean an exported picture is; MSAA off / SamplingMode 0 / MaxLodScale 0 was
# what beach-demo-003 shipped with (jagged edges, shimmering shadows, far detail popping -- the customer, 2026-09-30)
PICTURE_SETTINGS = ("MSAA", "FXAA_Enabled", "TXAA_Enabled", "SamplingMode", "MaxLodScale", "LodScale",
                    "ShadowQuality", "Shadow_SoftShadows", "UltraShadows_Enabled", "MotionBlurStrength")


def default_game_settings() -> Path:
    return Path.home() / "Documents" / "Rockstar Games" / "GTA V" / "settings.xml"


def check_game_settings(path) -> dict:
    """The game's settings.xml: Post FX at High or lower, or no lane is exported."""
    path = Path(path)
    if not path.is_file():
        raise ContractError(f"GTA settings.xml not found: {path} (name it with --game-settings)")
    try:
        root = ET.parse(path).getroot()
    except ET.ParseError as exc:
        raise ContractError(f"cannot read GTA settings.xml: {exc}") from None
    found = [node.attrib.get("value") for node in root.iter("PostFX")]
    if len(found) != 1 or found[0] is None or not found[0].isdigit():
        raise ContractError(f"GTA settings.xml must hold one PostFX value (found {found})")
    post_fx = int(found[0])
    if post_fx > MAX_POST_FX:
        name = POST_FX_NAMES[post_fx] if post_fx < len(POST_FX_NAMES) else str(post_fx)
        raise ContractError(f"the game's Post FX is {name}: an export would carry the recording camera's depth of "
                            f"field (near people out of focus). Set Settings > Graphics > Post FX to High, restart "
                            f"the game, and export again")
    # the picture settings an export renders under (aliasing, shimmer, far detail: the customer, 2026-09-30),
    # recorded with the lane so a package says what it was rendered with; absent in the file: null
    picture = {}
    for tag in PICTURE_SETTINGS:
        values = [node.attrib.get("value") for node in root.iter(tag)]
        picture[tag] = values[0] if len(values) == 1 else None
    return {"path": str(path.resolve()), "sha256": sha256_file(path), "post_fx": post_fx, "picture": picture}


MAX_TAKE_CLIPS = 64   # the plan's clip table (native kEditorExportPlanMaxClips)


@dataclass(frozen=True)
class TakeClip:
    path: Path
    sha256: str
    source_clip_name: str


@dataclass(frozen=True)
class TakeExportInputs:
    take_dir: Path
    replay: Path
    replay_sha256: str
    source_clip_name: str
    player_telemetry: Path
    player_telemetry_sha256: str
    tape_json: Path
    tape_bin: Path
    more_clips: tuple[TakeClip, ...] = ()   # clips 2..N of a take the recorder cut up


def take_export_inputs(take_dir, result: dict) -> TakeExportInputs:
    """What a recorded take hands its lane exports, each file checked against the
    take's own result. A take the recorder cut into several clips exports them in
    order as one Editor project: the first stands for the take (the player grid
    and every lane's binding name it), the rest follow it."""
    take_dir = Path(take_dir)
    clips = result.get("clips") or []
    if not clips or len(clips) > MAX_TAKE_CLIPS:
        raise ContractError(f"a take exports from one to {MAX_TAKE_CLIPS} clips; this one has {len(clips)}")
    sealed = []
    for index, clip in enumerate(clips, start=1):
        replay = take_dir / "source" / f"master-replay-{index}.clip"
        if clip.get("index") != index or not replay.is_file() or sha256_file(replay) != clip.get("sha256"):
            raise ContractError(f"the take's sealed replay {index} is missing or differs from its result")
        name = clip.get("source_clip_name")
        if not isinstance(name, str) or not name:
            raise ContractError(f"the take's result names no source clip for replay {index}")
        sealed.append(TakeClip(replay, clip["sha256"], name))
    telemetry = take_dir / "source" / "master-player-telemetry.jsonl"
    telemetry_sha = result.get("player_telemetry_sha256")
    if not telemetry.is_file() or sha256_file(telemetry) != telemetry_sha:
        raise ContractError("the take's player telemetry is missing or differs from its result")
    tape_json = take_dir / "config" / "camera-tape.json"
    tape_bin = take_dir / "config" / "camera-tape.bin"
    if not tape_json.is_file() or not tape_bin.is_file():
        raise ContractError("the take has no camera tape yet: run stage-take-tape first")
    # RE+ refuses a plan whose last frame lies past the end of the clips (every
    # clip loses a few frames at its ends and seams); say so before an export.
    last_frame_s = (int(load_json(tape_json)["n_frames"]) - 1) / 30
    held_s = sum(float(clip.get("duration_s") or 0) for clip in clips)
    if held_s + 0.0005 < last_frame_s:
        raise ContractError(f"the take's clips hold {held_s:.3f} s but the tape's last frame is at {last_frame_s:.3f} s; "
                            "the export would be refused: record the take again")
    first = sealed[0]
    return TakeExportInputs(take_dir, first.path, first.sha256, first.source_clip_name, telemetry, telemetry_sha,
                            tape_json, tape_bin, tuple(sealed[1:]))


# The Editor's pointer as it lands in a 1920 x 1080 exported frame, measured on
# that lane: '#' is its opaque dark outline, 'o' its opaque white fill. The other
# pixels blend with the scene and are not compared.
POINTER = (
    "####",
    "##.#",
    "##o.#",
    "##oo.#",
    "##ooo.#",
    "##oooo.#",
    "##ooooo.#",
    "##oooooo##",
    "##ooooooo##",
    "##oooooooo##",
    "##ooooooooo##",
    "##oooooooooo##",
    "##oooooooooo.##",
    "##ooooooooooo.##",
    "##oooooooooooo.##",
    "##ooooooooooooo.##",
    "##.ooooooooooooo.#",
    "##.oooooooooooooo.#",
    "##..ooooooo########",
)
_POINTER_CELLS = [(y, x, ch == "#") for y, row in enumerate(POINTER) for x, ch in enumerate(row) if ch in "#o"]
_POINTER_H = len(POINTER)
_POINTER_W = max(len(row) for row in POINTER)


def find_pointer(rgb):
    """The top-left corner of the Editor's pointer in one H x W x 3 frame, or None.

    Every seventh pointer cell must match exactly to be a candidate; a candidate
    is the pointer when nine in ten of all its cells match."""
    import numpy as np

    r, g, b = rgb[..., 0], rgb[..., 1], rgb[..., 2]
    dark = (r < 70) & (g < 70) & (b < 70)
    bright = (r > 200) & (g > 200) & (b > 200)
    rows, cols = dark.shape[0] - _POINTER_H + 1, dark.shape[1] - _POINTER_W + 1
    if rows <= 0 or cols <= 0:
        return None
    hits = np.ones((rows, cols), dtype=bool)
    for y, x, is_dark in _POINTER_CELLS[::7]:
        hits &= (dark if is_dark else bright)[y:y + rows, x:x + cols]
        if not hits.any():
            return None
    for top, left in zip(*np.nonzero(hits)):
        matched = sum(bool((dark if is_dark else bright)[top + y, left + x]) for y, x, is_dark in _POINTER_CELLS)
        if matched >= 0.9 * len(_POINTER_CELLS):
            return int(left), int(top)
    return None


def pointer_frames(pattern: str, count: int, *, workers: int | None = None) -> list[int]:
    """Every frame of a lane that shows the Editor's pointer. Decoding the PNGs is most of it and Pillow lets go
    of the GIL while it decodes, so the threads are as many as the machine's cores (four until 2026-10-13, when
    collecting took 2-3 minutes a lane, 24-36 of a group's)."""
    import os

    import numpy as np
    from PIL import Image

    workers = workers or max(4, min(16, os.cpu_count() or 4))

    def shows_pointer(index: int) -> bool:
        with Image.open(pattern.replace("%06d", f"{index:06d}")) as image:
            return find_pointer(np.asarray(image.convert("RGB"))) is not None

    with ThreadPoolExecutor(workers) as pool:
        return [i for i, shown in zip(range(count), pool.map(shows_pointer, range(count))) if shown]


def _lane_frames(lane_result: dict) -> tuple[str, int]:
    pattern = lane_result.get("frame_pattern")
    count = lane_result.get("frame_count")
    if not isinstance(pattern, str) or not isinstance(count, int) or count <= 0:
        raise ContractError("the lane result has no frame pattern and count")
    first = Path(pattern.replace("%06d", "000000"))
    last = Path(pattern.replace("%06d", f"{count - 1:06d}"))
    if not first.is_file() or not last.is_file():
        raise ContractError(f"the lane's frames are not all on disk ({first.name} .. {last.name})")
    return pattern, count


def check_lane_pixels(lane_result: dict) -> dict:
    """What the per-frame fence cannot see: the Editor's pointer drawn over the frames."""
    pattern, count = _lane_frames(lane_result)
    shown = pointer_frames(pattern, count)
    return {"checked_frames": count, "pointer_frames": len(shown),
            "first_pointer_frame": shown[0] if shown else None,
            "last_pointer_frame": shown[-1] if shown else None}


def _video_stream(path: Path, ffprobe: str) -> dict:
    probe = subprocess.run([ffprobe, "-v", "error", "-select_streams", "v:0", "-show_entries",
                            "stream=nb_frames,width,height,r_frame_rate", "-of", "json", str(path)],
                           check=True, capture_output=True, text=True)
    streams = json.loads(probe.stdout).get("streams") or [{}]
    return streams[0]


# H.264 encoders for the lane videos: libx264 on the CPU (what every delivery so far was), or the GPU's NVENC
# (2026-10-13, to take encoding off the capture machine's CPU and time: constant quality 16, the slowest,
# best preset; the GPU's encoder is its own block, so it does not draw on the rendering). Same container,
# frame count and rate either way; the delivery checks those, not the encoder.
VIDEO_ENCODERS = {
    "x264": ["-c:v", "libx264", "-preset", "slow", "-crf", "16"],
    "nvenc": ["-c:v", "h264_nvenc", "-preset", "p7", "-tune", "hq", "-rc", "vbr", "-cq", "16", "-b:v", "0"],
}


def encode_lane(lane_result: dict, out_path, *, fps: int = 30, audio=None, ffmpeg: str | None = None,
                skip: int = 0, encoder: str = "x264") -> Path:
    """The lane's PNG frames from frame `skip` on, exactly as rendered and every one of
    them, into one H.264 MP4. A take skips its hold (the tape's hold_frames): the
    video's frame 0 is then stage tick 0, and the sound is cut at the same moment."""
    if encoder not in VIDEO_ENCODERS:
        raise ContractError(f"unknown video encoder {encoder!r} ({', '.join(VIDEO_ENCODERS)})")
    pattern, count = _lane_frames(lane_result)
    if not isinstance(skip, int) or isinstance(skip, bool) or not 0 <= skip < count:
        raise ContractError(f"cannot start the video at frame {skip!r} of the lane's {count}")
    frames = count - skip
    tool = ffmpeg or shutil.which("ffmpeg")
    probe_tool = shutil.which("ffprobe", path=str(Path(tool).parent)) if tool else None
    if not tool or not probe_tool:
        raise ContractError("ffmpeg and ffprobe must be on PATH")
    out = Path(out_path)
    out.parent.mkdir(parents=True, exist_ok=True)
    argv = [tool, "-hide_banner", "-loglevel", "error", "-y", "-framerate", str(fps), "-start_number", str(skip),
            "-i", pattern]
    if audio and Path(audio).is_file():
        # The sound starts where the kept frames do and is padded or cut to exactly
        # their length. -shortest cut the last frame whenever the replay's audio
        # ended a few ms early.
        seconds = f"{frames / fps:.6f}"
        argv += ["-ss", f"{skip / fps:.6f}", "-i", str(audio), "-map", "0:v", "-map", "1:a",
                 "-af", f"apad=whole_dur={seconds},atrim=end={seconds}", "-c:a", "aac", "-b:a", "192k"]
    argv += ["-frames:v", str(frames), *VIDEO_ENCODERS[encoder], "-pix_fmt", "yuv420p", "-r", str(fps), str(out)]
    subprocess.run(argv, check=True)
    stream = _video_stream(out, probe_tool)
    if str(stream.get("nb_frames")) != str(frames) or stream.get("r_frame_rate") != f"{fps}/1":
        raise ContractError(f"{out.name} holds {stream.get('nb_frames')} frames at {stream.get('r_frame_rate')}, "
                            f"not the lane's {frames} from frame {skip} at {fps}/1")
    return out


def encode_quad(videos, out_path, *, ffmpeg: str | None = None) -> Path:
    """Four lanes side by side, 2 x 2, each at half size -- for looking, not delivery."""
    videos = [Path(v) for v in videos]
    if len(videos) != 4 or not all(v.is_file() for v in videos):
        raise ContractError("a quad preview needs the four lane videos")
    tool = ffmpeg or shutil.which("ffmpeg")
    if not tool:
        raise ContractError("ffmpeg is not on PATH")
    out = Path(out_path)
    out.parent.mkdir(parents=True, exist_ok=True)
    argv = [tool, "-hide_banner", "-loglevel", "error", "-y"]
    for v in videos:
        argv += ["-i", str(v)]
    argv += ["-filter_complex",
             "[0:v]scale=960:540[a];[1:v]scale=960:540[b];[2:v]scale=960:540[c];[3:v]scale=960:540[d];"
             "[a][b][c][d]xstack=inputs=4:layout=0_0|w0_0|0_h0|w0_h0[v]",
             "-map", "[v]", "-c:v", "libx264", "-crf", "20", "-pix_fmt", "yuv420p", str(out)]
    subprocess.run(argv, check=True)
    return out


def load_lane_result(take_dir, lane: int) -> dict:
    path = Path(take_dir) / "evidence" / f"native-lane-{lane}-result.json"
    if not path.is_file():
        raise ContractError(f"lane {lane} has not been exported")
    value = load_json(path)
    lane_result = value.get("lane") if isinstance(value, dict) else None
    if not isinstance(lane_result, dict) or value.get("ok") is not True:
        raise ContractError(f"lane {lane} did not export")
    pixels = value.get("pixel_check")
    if not isinstance(pixels, dict) or pixels.get("pointer_frames") != 0:
        raise ContractError(f"lane {lane} has no clean pixel check (Editor pointer on frames): export it again")
    return lane_result
