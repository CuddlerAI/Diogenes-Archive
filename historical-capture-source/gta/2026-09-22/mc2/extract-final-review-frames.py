"""Read the sealed final MP4 and extract six exact decoded frames to a new directory."""
from pathlib import Path
from datetime import datetime, timezone
import hashlib
import json
import subprocess


def sha(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1048576), b""):
            value.update(chunk)
    return value.hexdigest()


base = Path(__file__).resolve().parent
run = base / "run-mc2-beach-002-reexport-02"
source = run / "preview/mc2-final/four-camera-preview.mp4"
receipt_path = source.parent / "review-media-manifest.json"
receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
record = next(r for r in receipt["outputs"] if Path(r["path"]).resolve() == source)
assert source.stat().st_size == record["bytes"] and sha(source) == record["sha256"]
manifest = json.loads((run / "config/dependency-manifest.json").read_text(encoding="utf-8"))
ffmpeg = Path(manifest["components"]["ffmpeg"]["path"])
assert sha(ffmpeg) == manifest["components"]["ffmpeg"]["sha256"]

out = base / "review/retry02-final-mp4-frames"
out.mkdir(parents=True, exist_ok=False)
indices = [0, 240, 600, 1080, 1440, 1797]
select = "select=" + "+".join(r"eq(n\,%d)" % frame for frame in indices)
command = [str(ffmpeg), "-hide_banner", "-loglevel", "error", "-nostdin", "-n",
           "-threads", "2", "-i", str(source), "-vf", select,
           "-fps_mode", "vfr", "-frames:v", "6", "-threads", "1",
           str(out / "sample-%02d.png")]
result = subprocess.run(command, capture_output=True, text=True,
                        encoding="utf-8", errors="replace")
if result.returncode:
    raise SystemExit(result.stderr)
assert sha(source) == record["sha256"]
images = []
for number, frame in enumerate(indices, 1):
    path = out / ("sample-%02d.png" % number)
    assert path.is_file()
    images.append({"path": str(path), "bytes": path.stat().st_size,
                   "sha256": sha(path), "decoded_frame_index": frame,
                   "source_time_s": frame / 30})
report = {
    "schema": "dm-gta-final-preview-frame-sample/1",
    "created_at": datetime.now(timezone.utc).isoformat(),
    "source": record, "source_unchanged_after_extraction": True,
    "source_receipt": {"path": str(receipt_path), "sha256": sha(receipt_path)},
    "ffmpeg": manifest["components"]["ffmpeg"], "command": command,
    "outputs": images,
    "selection": "Exact decoded frame indices 0,240,600,1080,1440,1797; 6 samples only",
    "human_review_approval": False, "full_video_visual_approval": False,
    "calibration_approval": False,
}
with (out / "extraction-receipt.json").open("x", encoding="utf-8", newline="\n") as stream:
    json.dump(report, stream, ensure_ascii=False, indent=2)
    stream.write("\n")
print(json.dumps({"status": "completed", "output_dir": str(out),
                  "frames": indices, "source_unchanged": True}, ensure_ascii=False))
