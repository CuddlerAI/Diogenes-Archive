"""Build derived review media from real, hash-sealed GTA lane frames.

No native frame, capture video, run manifest or approval is modified. By default,
write 16-sample contact sheets and same-frame four-view stills. --quad additionally
encodes a 1920x1080/30fps preview, using four uncropped 960x540 tiles. Its duration
comes from the frame count (1800 frames = 60s), never a padded or looped clip.
--lane checks one completed lane early; --validate-only writes input validation
only and performs no image rendering or video encoding.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import math
import subprocess
import sys
from datetime import datetime, timezone
from fractions import Fraction
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


FPS = 30
SAMPLES = 16
SIZE = (1920, 1080)


class ReviewError(RuntimeError):
    pass


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def file_record(path):
    path = Path(path)
    return {"path": str(path.resolve()), "bytes": path.stat().st_size, "sha256": digest(path)}


def read_json(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))


def write_json_new(path, value):
    with Path(path).open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(value, stream, indent=2, ensure_ascii=False)
        stream.write("\n")


def sha_valid(value):
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def sample_indices(frame_count):
    count = min(SAMPLES, frame_count)
    return [0] if count == 1 else [round(i * (frame_count - 1) / (count - 1)) for i in range(count)]


def read_lane(root, lane):
    path = root / "evidence" / f"frame-events-lane{lane}.jsonl"
    rows = []
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            if line.strip():
                rows.append(json.loads(line))
    if not rows:
        raise ReviewError(f"lane {lane} has no sealed frame events")
    if not all(isinstance(row, dict) for row in rows):
        raise ReviewError(f"lane {lane} contains a non-object frame event")
    replay = rows[0].get("replay_sha256")
    if not sha_valid(replay):
        raise ReviewError(f"lane {lane} has no valid replay hash")
    for frame, row in enumerate(rows):
        if (not isinstance(row, dict) or row.get("schema") != "dm-gta-frame/1"
                or row.get("lane") != lane or row.get("output_frame") != frame
                or row.get("replay_frame") != frame
                or row.get("source_t_ns") != round(frame * 1e9 / FPS)
                or row.get("replay_sha256") != replay
                or row.get("mock") is not False or row.get("render_succeeded") is not True
                or not sha_valid(row.get("output_sha256"))
                or not isinstance(row.get("output_bytes"), int) or row["output_bytes"] <= 0
                or not isinstance(row.get("output_path"), str)
                or not Path(row["output_path"]).is_absolute()):
            raise ReviewError(f"lane {lane}, frame {frame}: invalid real frame/timeline/hash evidence")
    return rows, file_record(path)


def sealed_png(row, *, render=False, size=None):
    path = Path(row["output_path"])
    data = path.read_bytes()
    if len(data) != row["output_bytes"] or hashlib.sha256(data).hexdigest() != row["output_sha256"]:
        raise ReviewError(f"PNG changed from sealed bytes/sha256: {path}")
    # Decode the exact bytes that were hashed, not a second unsealed filesystem read.
    with Image.open(io.BytesIO(data)) as image:
        if image.format != "PNG" or image.width * 9 != image.height * 16:
            raise ReviewError(f"native frame is not a 16:9 PNG: {path}")
        if not render:
            return image.size
        image = image.convert("RGB")
        return image.resize(size, Image.Resampling.LANCZOS) if size else image.copy()


def font_path():
    for candidate in (Path("C:/Windows/Fonts/arial.ttf"),
                      Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")):
        if candidate.is_file():
            return candidate
    raise ReviewError("a readable Arial/DejaVu font is required for derived review labels")


def label(image, lines, *, size=14):
    draw = ImageDraw.Draw(image)
    font = ImageFont.truetype(str(font_path()), size)
    text = "\n".join(lines)
    bounds = draw.multiline_textbbox((10, 8), text, font=font, spacing=3)
    draw.rectangle((bounds[0] - 4, bounds[1] - 3, bounds[2] + 4, bounds[3] + 3), fill="#171717")
    draw.multiline_text((10, 8), text, font=font, fill="white", spacing=3)


def make_stills(out, all_rows, indices):
    outputs = []
    for lane, rows in all_rows.items():
        sheet = Image.new("RGB", SIZE, "black")
        for slot, frame in enumerate(indices):
            tile = sealed_png(rows[frame], render=True, size=(480, 270))
            label(tile, [f"CAM {lane}  {frame / FPS:.3f}s  FRAME {frame}",
                         "INTERNAL / NON-TRAINABLE"], size=12)
            sheet.paste(tile, ((slot % 4) * 480, (slot // 4) * 270))
        path = out / f"camera-{lane}-contact.jpg"
        sheet.save(path, quality=94, subsampling=0)
        outputs.append(file_record(path))
    if set(all_rows) == {1, 2, 3, 4}:
        for frame in indices:
            sheet = Image.new("RGB", SIZE)
            for lane, rows in all_rows.items():
                tile = sealed_png(rows[frame], render=True, size=(960, 540))
                label(tile, [f"CAM {lane}  {frame / FPS:.3f}s  FRAME {frame}",
                             "INTERNAL / NON-TRAINABLE"], size=15)
                sheet.paste(tile, (((lane - 1) % 2) * 960, ((lane - 1) // 2) * 540))
            path = out / f"four-view-frame-{frame:06d}.jpg"
            sheet.save(path, quality=94, subsampling=0)
            outputs.append(file_record(path))
    return outputs


def pinned_tools(root):
    manifest_path = root / "config" / "dependency-manifest.json"
    manifest = read_json(manifest_path)
    tools = {}
    for name in ("ffmpeg", "ffprobe"):
        record = manifest.get("components", {}).get(name, {})
        path = Path(record.get("path", ""))
        if (not path.is_absolute() or not path.is_file() or not sha_valid(record.get("sha256"))
                or digest(path) != record["sha256"]):
            raise ReviewError(f"manifest-pinned {name} is missing or its sha256 changed")
        tools[name] = str(path)
    return tools, file_record(manifest_path)


def run_tool(command):
    result = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    if result.returncode:
        raise ReviewError(f"{Path(command[0]).name} failed ({result.returncode}): {result.stderr[-3000:]}")
    return result.stdout


def probe_video(ffprobe, path, *, decode_count=False):
    command = [ffprobe, "-v", "error", "-threads", "1"]
    if decode_count:
        command.append("-count_frames")
    command += ["-show_streams", "-show_format", "-of", "json", str(path)]
    return json.loads(run_tool(command))


def validate_video(probe, frame_count, *, require_output_size=False):
    videos = [stream for stream in probe.get("streams", []) if stream.get("codec_type") == "video"]
    if len(videos) != 1:
        raise ReviewError("media must contain exactly one video stream")
    video = videos[0]
    width, height = video.get("width"), video.get("height")
    if (not isinstance(width, int) or not isinstance(height, int) or width * 9 != height * 16
            or (require_output_size and (width, height) != SIZE)):
        raise ReviewError(f"video dimensions are not the expected uncropped 16:9 size: {width}x{height}")
    if Fraction(video.get("r_frame_rate", "0")) != FPS or Fraction(video.get("avg_frame_rate", "0")) != FPS:
        raise ReviewError("video must have 30/1 reported and average frame rate")
    count = video.get("nb_read_frames") if require_output_size else video.get("nb_frames")
    if count is not None and count != "N/A" and int(count) != frame_count:
        raise ReviewError(f"video frame count {count} differs from sealed {frame_count}")
    if require_output_size and (count is None or count == "N/A"):
        raise ReviewError("final preview has no exact decoded frame count")
    duration = float(video.get("duration", "nan"))
    if not math.isfinite(duration) or abs(duration - frame_count / FPS) > 1 / FPS:
        raise ReviewError("video duration differs from the sealed frame timeline")
    if video.get("sample_aspect_ratio") not in (None, "N/A", "1:1"):
        raise ReviewError("non-square source pixels would distort a 960x540 tile")
    return {"width": width, "height": height, "frame_count": int(count) if count not in (None, "N/A") else None,
            "fps": "30/1", "duration_s": duration,
            "has_audio": any(s.get("codec_type") == "audio" for s in probe.get("streams", []))}


def prepare_quad(root, frame_count):
    tools, manifest_record = pinned_tools(root)
    videos = {}
    for lane in range(1, 5):
        path = root / "capture" / f"video_{lane}.mp4"
        if not path.is_file():
            raise ReviewError(f"quad requires materialized capture/video_{lane}.mp4")
        probe = probe_video(tools["ffprobe"], path)
        video = validate_video(probe, frame_count)
        if video["frame_count"] is None:
            video = validate_video(probe_video(tools["ffprobe"], path, decode_count=True),
                                   frame_count, require_output_size=True)
        videos[lane] = {"file": file_record(path), "probe": video}
    return tools, manifest_record, videos


def quad_filter():
    # Only fixed label text is interpolated into the filter. Font path quoting
    # follows FFmpeg's filter grammar; no shell is involved.
    font = str(font_path()).replace("\\", "/").replace(":", "\\:").replace("'", "\\'")
    filters = []
    for lane in range(4):
        filters.append(f"[{lane}:v:0]scale=960:540:flags=lanczos,setsar=1,setpts=PTS-STARTPTS,"
                       f"drawtext=fontfile='{font}':text='CAM {lane + 1}':fontsize=18:"
                       f"fontcolor=white:box=1:boxcolor=black@0.65:boxborderw=4:x=12:y=12[c{lane}]")
    filters.append("[c0][c1][c2][c3]xstack=inputs=4:layout=0_0|960_0|0_540|960_540,"
                   f"drawtext=fontfile='{font}':text='INTERNAL / NON-TRAINABLE':fontsize=14:"
                   "fontcolor=white:box=1:boxcolor=black@0.65:boxborderw=4:x=12:y=h-th-12,"
                   "format=yuv420p[v]")
    return ";".join(filters)


def encode_quad(out, tools, videos, frame_count):
    target = out / "four-camera-preview.mp4"
    partial = out / "four-camera-preview.partial.mp4"
    command = [tools["ffmpeg"], "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
               "-filter_complex_threads", "1"]
    for lane in range(1, 5):
        command += ["-threads", "1", "-i", videos[lane]["file"]["path"]]
    command += ["-filter_complex", quad_filter(), "-map", "[v]"]
    if videos[1]["probe"]["has_audio"]:
        command += ["-map", "0:a:0", "-c:a", "aac", "-b:a", "160k"]
    else:
        command += ["-an"]
    command += ["-c:v", "libx264", "-threads", "2", "-preset", "medium", "-crf", "20",
                "-pix_fmt", "yuv420p", "-r", "30", "-fps_mode", "cfr",
                "-frames:v", str(frame_count), "-t", f"{frame_count / FPS:.9f}",
                "-movflags", "+faststart", str(partial)]
    run_tool(command)
    probe = probe_video(tools["ffprobe"], partial, decode_count=True)
    verified = validate_video(probe, frame_count, require_output_size=True)
    for entry in videos.values():
        if file_record(entry["file"]["path"]) != entry["file"]:
            raise ReviewError("a source capture video changed during preview encoding")
    if target.exists():
        raise ReviewError("preview target already exists; originals and previous products are never overwritten")
    partial.rename(target)
    return file_record(target), verified, probe


def reserve_output(root, requested, frame_roots):
    out = (Path(requested).resolve() if requested else root / "preview" /
           datetime.now(timezone.utc).strftime("review-%Y%m%dT%H%M%S.%fZ"))
    protected = [root / name for name in ("source", "config", "evidence", "capture")]
    protected += list(frame_roots)
    if out == root or any(out == path or out.is_relative_to(path) or path.is_relative_to(out)
                          for path in protected):
        raise ReviewError("output must be separate from native frames, source artifacts and capture videos")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        raise ReviewError("output directory must be new or empty; previous products are never overwritten")
    out.mkdir(parents=True, exist_ok=True)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", required=True, type=Path)
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument("--lane", type=int, choices=(1, 2, 3, 4))
    parser.add_argument("--quad", action="store_true", help="also encode and verify a 2x2 MP4")
    parser.add_argument("--validate-only", action="store_true", help="only validate inputs; no rendering/encoding")
    args = parser.parse_args()
    if args.lane and args.quad:
        parser.error("--quad requires all four lanes; omit --lane")
    root = args.run_dir.resolve()
    lanes = [args.lane] if args.lane else list(range(1, 5))
    all_rows, event_records = {}, {}
    for lane in lanes:
        all_rows[lane], event_records[str(lane)] = read_lane(root, lane)
    counts = {len(rows) for rows in all_rows.values()}
    replays = {rows[0]["replay_sha256"] for rows in all_rows.values()}
    if len(counts) != 1 or len(replays) != 1:
        raise ReviewError("selected lanes do not share one frame count and sealed replay")
    frame_count = next(iter(counts))
    indices = sample_indices(frame_count)
    selected = {}
    frame_roots = set()
    for lane, rows in all_rows.items():
        selected[str(lane)] = []
        for frame in indices:
            row = rows[frame]
            size = sealed_png(row)
            frame_roots.add(Path(row["output_path"]).parent.resolve())
            selected[str(lane)].append({"frame": frame, "source_t_ns": row["source_t_ns"],
                "path": row["output_path"], "bytes": row["output_bytes"],
                "sha256": row["output_sha256"], "native_dimensions": list(size)})
    tools = manifest_record = videos = None
    if args.quad:
        tools, manifest_record, videos = prepare_quad(root, frame_count)
    out = reserve_output(root, args.out_dir, frame_roots)
    report = {"schema": "gta-derived-review/2", "run_dir": str(root),
              "created_at": datetime.now(timezone.utc).isoformat(),
              "status": "inputs_validated_only" if args.validate_only else "completed",
              "non_trainable": True, "formal_pr405_pass": False, "human_review_approval": False,
              "source_frames_unchanged": True, "source_videos_unchanged": True,
              "replay_sha256": next(iter(replays)), "frame_count": frame_count,
              "fps": "30/1", "duration_s": frame_count / FPS,
              "sample_indices": indices, "event_files": event_records, "selected_frames": selected,
              "method": "16 evenly spaced sealed frames per lane; every shown PNG hash verified; "
                        "review derivatives do not prove full-frame collision/occlusion absence",
              "preview_layout": {"width": 1920, "height": 1080, "tile_width": 960,
                                 "tile_height": 540, "crop": False, "padding": False},
              "media_generated": not args.validate_only, "outputs": []}
    if args.quad:
        report.update(dependency_manifest=manifest_record,
                      source_videos={str(k): v for k, v in videos.items()})
    if not args.validate_only:
        report["outputs"] = make_stills(out, all_rows, indices)
        if args.quad:
            artifact, verified, probe = encode_quad(out, tools, videos, frame_count)
            report["outputs"].append(artifact)
            report["preview_probe_verified"] = verified
            write_json_new(out / "preview-ffprobe.json", probe)
            report["outputs"].append(file_record(out / "preview-ffprobe.json"))
    # Do not publish a completion receipt if evidence changed during processing.
    for record in event_records.values():
        if file_record(record["path"]) != record:
            raise ReviewError("a source frame-event file changed during review generation")
    name = "input-validation.json" if args.validate_only else "review-media-manifest.json"
    write_json_new(out / name, report)
    print(json.dumps({"status": report["status"], "output_dir": str(out),
                      "frame_count": frame_count, "duration_s": frame_count / FPS,
                      "samples_per_lane": len(indices), "lanes": lanes,
                      "media_generated": report["media_generated"],
                      "output_count": len(report["outputs"]), "receipt": str(out / name)}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ReviewError, OSError, ValueError, TypeError, KeyError) as exc:
        print(json.dumps({"status": "failed", "error": str(exc), "completion_receipt_written": False}),
              file=sys.stderr)
        raise SystemExit(2)
