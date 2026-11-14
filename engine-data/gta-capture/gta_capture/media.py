"""Normalize exporter output into the four docs/69 MP4 files."""

from __future__ import annotations

import json
import math
import os
import shutil
import stat
import subprocess
from fractions import Fraction
from pathlib import Path

from .errors import BackendError, ContractError
from .jsonio import sha256_file


def _run(command: list[str]) -> None:
    proc = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, encoding="utf-8", errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(proc.stderr.splitlines()[-20:])
        raise BackendError(f"media command failed ({proc.returncode}): {' '.join(command)}\n{tail}")


def encode_frame_sequence(ffmpeg: str, frame_pattern: str, out_path: Path, *,
                          n_frames: int, fps: int = 30) -> Path:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    _run([
        ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
        "-framerate", str(fps), "-i", frame_pattern,
        "-frames:v", str(n_frames),
        "-an", "-c:v", "libx264", "-preset", "slow", "-crf", "15",
        "-pix_fmt", "yuv420p", "-r", str(fps), str(out_path),
    ])
    return out_path


def _ffprobe_for(ffmpeg: str) -> str:
    executable = Path(ffmpeg)
    suffix = executable.suffix if executable.suffix else (".exe" if shutil.which("ffprobe.exe") else "")
    if executable.parent != Path("."):
        sibling = executable.with_name(f"ffprobe{suffix}")
        if sibling.is_file():
            return str(sibling)
    found = shutil.which("ffprobe") or shutil.which("ffprobe.exe")
    if found:
        return found
    raise ContractError("ffprobe is required to verify materialized media")


def _probe_media(ffprobe: str, path: Path) -> dict:
    proc = subprocess.run([
        ffprobe, "-v", "error", "-count_frames", "-show_streams", "-show_format",
        "-of", "json", str(path),
    ], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, encoding="utf-8", errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(proc.stderr.splitlines()[-20:])
        raise BackendError(f"ffprobe failed ({proc.returncode}) for {path}\n{tail}")
    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise BackendError(f"ffprobe returned invalid JSON for {path}") from exc
    if not isinstance(result, dict):
        raise BackendError(f"ffprobe returned an invalid report for {path}")
    return result


def _fraction(value, label: str) -> Fraction:
    if not isinstance(value, str):
        raise ContractError(f"muxed video has no {label}")
    try:
        result = Fraction(value)
    except (ValueError, ZeroDivisionError) as exc:
        raise ContractError(f"muxed video has invalid {label}: {value!r}") from exc
    if result <= 0:
        raise ContractError(f"muxed video has non-positive {label}")
    return result


def _stream_duration(stream: dict, label: str) -> float:
    value = stream.get("duration")
    try:
        result = float(value)
    except (TypeError, ValueError) as exc:
        raise ContractError(f"muxed {label} stream has no duration") from exc
    if not math.isfinite(result) or result <= 0:
        raise ContractError(f"muxed {label} stream has invalid duration")
    return result


def _verify_muxed_output(report: dict, *, n_frames: int, fps: int,
                         expected_duration_s: float) -> None:
    streams = report.get("streams")
    if not isinstance(streams, list):
        raise ContractError("muxed output has no ffprobe streams")
    videos = [item for item in streams
              if isinstance(item, dict) and item.get("codec_type") == "video"]
    audios = [item for item in streams
              if isinstance(item, dict) and item.get("codec_type") == "audio"]
    if len(videos) != 1 or len(audios) != 1:
        raise ContractError("muxed output must contain exactly one video and one audio stream")
    video = videos[0]
    audio = audios[0]
    try:
        measured_frames = int(video.get("nb_read_frames"))
    except (TypeError, ValueError) as exc:
        raise ContractError("muxed video has no exact decoded frame count") from exc
    if measured_frames != n_frames:
        raise ContractError(
            f"muxed video frame count {measured_frames} differs from sealed {n_frames}")
    target_rate = Fraction(fps, 1)
    if (_fraction(video.get("avg_frame_rate"), "average frame rate") != target_rate
            or _fraction(video.get("r_frame_rate"), "real frame rate") != target_rate):
        raise ContractError(f"muxed video is not exact {fps} FPS CFR")
    if audio.get("sample_rate") != "48000" or audio.get("channels") != 2:
        raise ContractError("muxed audio must be 48 kHz stereo")

    # Container time bases and AAC priming can quantize the exposed duration. One
    # video tick / AAC frame is the maximum permissible representation error.
    tolerance_s = max(1.0 / fps, 1024.0 / 48000.0) + 1e-6
    for label, stream in (("video", video), ("audio", audio)):
        duration_s = _stream_duration(stream, label)
        if abs(duration_s - expected_duration_s) > tolerance_s:
            raise ContractError(
                f"muxed {label} duration {duration_s:.6f}s differs from "
                f"sealed {expected_duration_s:.6f}s")
    format_value = report.get("format")
    if not isinstance(format_value, dict):
        raise ContractError("muxed output has no container report")
    try:
        container_duration_s = float(format_value.get("duration"))
    except (TypeError, ValueError) as exc:
        raise ContractError("muxed output has no container duration") from exc
    if (not math.isfinite(container_duration_s)
            or abs(container_duration_s - expected_duration_s) > tolerance_s):
        raise ContractError("muxed container duration differs from sealed frame duration")


def mux_master_audio(ffmpeg: str, video_path: Path, audio_path: Path, out_path: Path, *,
                     n_frames: int, fps: int = 30, ffprobe: str | None = None) -> Path:
    if n_frames <= 0 or fps <= 0:
        raise ContractError("mux requires positive sealed frame count and FPS")
    duration_s = n_frames / float(fps)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    temp = out_path.with_name(out_path.name + ".muxing.mp4")
    temp.unlink(missing_ok=True)
    try:
        _run([
            ffmpeg, "-hide_banner", "-loglevel", "error", "-y",
            "-i", str(video_path), "-i", str(audio_path),
            "-filter_complex", f"[1:a:0]apad,atrim=duration={duration_s:.12f},"
            "asetpts=PTS-STARTPTS[a]",
            "-map", "0:v:0", "-map", "[a]", "-c:v", "copy",
            "-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-ac", "2",
            "-movflags", "+faststart", str(temp),
        ])
        report = _probe_media(ffprobe or _ffprobe_for(ffmpeg), temp)
        _verify_muxed_output(
            report, n_frames=n_frames, fps=fps, expected_duration_s=duration_s)
        temp.replace(out_path)
    finally:
        temp.unlink(missing_ok=True)
    return out_path


def _file_identity(path: Path) -> tuple[int, int, int]:
    info = path.lstat()
    if (stat.S_ISLNK(info.st_mode)
            or getattr(info, "st_file_attributes", 0)
            & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0)):
        raise ContractError(f"media scratch path is redirected: {path}")
    return info.st_dev, info.st_ino, stat.S_IFMT(info.st_mode)


def _require_owned_path(path: Path, expected: Path, identity: tuple[int, int, int]) -> None:
    # Check the resolved absolute target and its identity before any cleanup.
    # A replaced directory, junction or symlink must not turn scratch cleanup
    # into deletion of RAW, an earlier failed attempt, or another output.
    if (not expected.is_absolute() or path != expected
            or path.resolve(strict=True) != expected
            or _file_identity(path) != identity):
        raise ContractError(f"media scratch path escaped or changed ownership: {path}")


def _copy_new_file(source: Path, destination: Path) -> None:
    # Exclusive creation never overwrites an existing file or follows an
    # existing destination link. Both source and copied bytes are checked by
    # the caller against the original seal.
    with source.open("rb") as reader, destination.open("xb") as writer:
        shutil.copyfileobj(reader, writer, length=1024 * 1024)


def _copy_sealed_audio(artifact: dict, scratch: Path) -> tuple[Path, dict]:
    value = artifact.get("audio_path")
    expected_sha = artifact.get("audio_sha256")
    expected_bytes = artifact.get("audio_bytes")
    if (not isinstance(value, str) or not isinstance(expected_sha, str)
            or len(expected_sha) != 64):
        raise ContractError("master audio path/sha256 seal is missing")
    source = Path(value).resolve()
    if not source.is_file():
        raise ContractError("sealed master audio is missing")
    measured_bytes = source.stat().st_size
    if (expected_bytes is not None
            and (not isinstance(expected_bytes, int) or isinstance(expected_bytes, bool)
                 or expected_bytes != measured_bytes)):
        raise ContractError("master audio byte count differs from its seal")
    if sha256_file(source) != expected_sha.casefold():
        raise ContractError("master audio sha256 differs from its seal")
    destination = scratch / "master-audio.wav"
    _copy_new_file(source, destination)
    if (destination.stat().st_size != measured_bytes
            or sha256_file(destination) != expected_sha.casefold()
            or source.stat().st_size != measured_bytes
            or sha256_file(source) != expected_sha.casefold()):
        raise ContractError("master audio staging copy is not sealed")
    return destination, {"bytes": measured_bytes, "sha256": expected_sha.casefold()}


def _verify_staged_audio(path: Path, seal: dict) -> None:
    if (not path.is_file() or path.stat().st_size != seal["bytes"]
            or sha256_file(path) != seal["sha256"]):
        raise ContractError("staged master audio changed during media encoding")


def _copy_sealed_frames(artifact: dict, rows: list[dict], scratch: Path,
                        lane: int) -> tuple[str, int, list[dict], tuple[int, int, int]]:
    pattern = artifact.get("frame_pattern")
    if not isinstance(pattern, str) or "%" not in pattern:
        raise ContractError(f"lane {lane} frame_pattern is missing or not indexed")
    if artifact.get("frame_count") != len(rows) or not rows:
        raise ContractError(f"lane {lane} frame_count differs from sealed events")
    lane_dir = scratch / f"lane-{lane}-frames"
    lane_dir.mkdir()
    lane_identity = _file_identity(lane_dir)
    copied = []
    extension = None
    for index, row in enumerate(rows):
        try:
            patterned = Path(pattern % index).resolve()
        except (TypeError, ValueError) as exc:
            raise ContractError(f"lane {lane} frame_pattern cannot format index {index}") from exc
        source_value = row.get("output_path")
        if not isinstance(source_value, str) or patterned != Path(source_value).resolve():
            raise ContractError(
                f"lane {lane} frame {index} pattern is not bound to sealed RGB evidence")
        source = patterned
        expected_sha = row.get("output_sha256")
        expected_bytes = row.get("output_bytes")
        if (not source.is_file() or source.stat().st_size != expected_bytes
                or sha256_file(source) != expected_sha):
            raise ContractError(f"lane {lane} frame {index} changed before media staging")
        if extension is None:
            extension = source.suffix.casefold()
        elif source.suffix.casefold() != extension:
            raise ContractError(f"lane {lane} mixes image extensions")
        destination = lane_dir / f"frame_{index:06d}{extension}"
        _require_owned_path(lane_dir, scratch / f"lane-{lane}-frames", lane_identity)
        _copy_new_file(source, destination)
        if (destination.stat().st_size != expected_bytes
                or sha256_file(destination) != expected_sha):
            raise ContractError(f"lane {lane} frame {index} staging copy is not sealed")
        copied.append({"path": destination, "bytes": expected_bytes,
                       "sha256": expected_sha, "identity": _file_identity(destination)})
    assert extension is not None
    return str(lane_dir / f"frame_%06d{extension}"), len(rows), copied, lane_identity


def _verify_staged_frames(copied: list[dict], lane: int) -> None:
    for index, item in enumerate(copied):
        path = item["path"]
        if (not path.is_file() or path.stat().st_size != item["bytes"]
                or sha256_file(path) != item["sha256"]):
            raise ContractError(f"lane {lane} staged frame {index} changed during encoding")


def _release_staged_lane(scratch: Path, scratch_identity: tuple[int, int, int],
                         lane: int, lane_identity: tuple[int, int, int],
                         copied: list[dict], silent_video: Path,
                         silent_identity: tuple[int, int, int]) -> None:
    _require_owned_path(scratch, scratch, scratch_identity)
    lane_dir = scratch / f"lane-{lane}-frames"
    _require_owned_path(lane_dir, lane_dir, lane_identity)
    paths = {item["path"] for item in copied}
    if len(paths) != len(copied) or set(lane_dir.iterdir()) != paths:
        raise ContractError(f"lane {lane} scratch contains unowned files; leaving it intact")
    for item in copied:
        _require_owned_path(item["path"], lane_dir / item["path"].name, item["identity"])
    _require_owned_path(silent_video, scratch / f"lane-{lane}-silent.mp4", silent_identity)
    # No recursive deletion: remove only files created and verified by this
    # call. An unexpected entry prevents even the empty-directory removal.
    for item in copied:
        item["path"].unlink()
    lane_dir.rmdir()
    silent_video.unlink()


def materialize_videos(capture_dir: Path, lane_artifacts: dict[int, dict], audio_artifact: dict,
                       *, ffmpeg: str,
                       lane_events: dict[int, list[dict]] | None = None,
                       ffprobe: str | None = None, fps: int = 30) -> list[str]:
    outputs = []
    run_root = capture_dir.absolute().parent.resolve()
    capture_dir = run_root / capture_dir.name
    if capture_dir.resolve() != capture_dir:
        raise ContractError("media capture directory is redirected outside its run")
    scratch = run_root / "media-scratch"
    if os.path.lexists(scratch):
        raise ContractError("media-scratch already exists; preserve the earlier attempt")
    for lane in range(1, 5):
        out = capture_dir / f"video_{lane}.mp4"
        if os.path.lexists(out) or os.path.lexists(out.with_name(out.name + ".muxing.mp4")):
            raise ContractError(f"lane {lane} media output already exists; preserve the earlier attempt")
    scratch.mkdir(parents=True)
    scratch_identity = _file_identity(scratch)
    audio, audio_seal = _copy_sealed_audio(audio_artifact, scratch)
    audio_identity = _file_identity(audio)
    for lane in range(1, 5):
        artifact = lane_artifacts[lane]
        video_value = artifact.get("video_path")
        if isinstance(video_value, str) and Path(video_value).is_file():
            raise ContractError(
                f"lane {lane} pre-encoded video bypasses per-frame RGB sealing")
        if lane_events is None or lane not in lane_events:
            raise ContractError(f"lane {lane} has no sealed frame events for media encoding")
        _require_owned_path(scratch, scratch, scratch_identity)
        pattern, n_frames, copied, lane_identity = _copy_sealed_frames(
            artifact, lane_events[lane], scratch, lane)
        silent_path = scratch / f"lane-{lane}-silent.mp4"
        # Reserve this call's output before invoking FFmpeg's existing -y path.
        with silent_path.open("xb"):
            pass
        silent_identity = _file_identity(silent_path)
        silent_video = encode_frame_sequence(
            ffmpeg, pattern, silent_path,
            n_frames=n_frames, fps=fps)
        _require_owned_path(silent_video, silent_path, silent_identity)
        _verify_staged_frames(copied, lane)
        _verify_staged_audio(audio, audio_seal)
        out = capture_dir / f"video_{lane}.mp4"
        if (capture_dir.resolve() != capture_dir or os.path.lexists(out)
                or os.path.lexists(out.with_name(out.name + ".muxing.mp4"))):
            raise ContractError(f"lane {lane} media output appeared or was redirected during encoding")
        mux_master_audio(ffmpeg, silent_video, audio, out,
                         n_frames=n_frames, fps=fps, ffprobe=ffprobe)
        _verify_staged_audio(audio, audio_seal)
        _verify_staged_frames(copied, lane)
        outputs.append(str(out.resolve()))
        _release_staged_lane(scratch, scratch_identity, lane, lane_identity,
                             copied, silent_video, silent_identity)
    # Only the current failed lane is retained on failure. Successful lanes
    # release their verified copies immediately, bounding PNG scratch to one
    # lane while all original RAW paths and completed videos remain intact.
    _require_owned_path(scratch, scratch, scratch_identity)
    _require_owned_path(audio, scratch / "master-audio.wav", audio_identity)
    if set(scratch.iterdir()) != {audio}:
        raise ContractError("media scratch contains unowned files; leaving it intact")
    audio.unlink()
    scratch.rmdir()
    return outputs
