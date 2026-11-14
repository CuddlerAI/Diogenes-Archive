"""verify_clean.py <root> <group> <take> <lane> [--delete]: check a lane's encoded video against its exported frames
-- frames = PNGs - hold, 1920x1080, one audio stream, the length right -- and with --delete remove that lane's render
folder (<root>\\renders\\render_NNNN) once every check passes. Run on the capture PC from gta-capture (lane.sh encode).
A render folder's name is reused by later exports: only the folder the lane's own result names is touched."""
import json, pathlib, re, shutil, subprocess, sys
root = pathlib.Path(sys.argv[1]); G = root / "groups" / sys.argv[2]
take, lane = sys.argv[3], int(sys.argv[4])
T = G / "takes" / take
res = json.loads((T / "evidence" / f"native-lane-{lane}-result.json").read_text(encoding="utf-8"))
ln = res.get("lane") or res
pattern, count = ln["frame_pattern"], int(ln["frame_count"])
render = pathlib.Path(pattern).parent
assert render.parent == root / "renders" and re.fullmatch(r"render_\d{4}", render.name), render
hold = int(json.loads((T / "config" / "camera-tape.json").read_text(encoding="utf-8"))["source"]["hold_frames"])
video = T / "capture" / f"video_{lane}.mp4"
assert video.is_file(), video
pngs = len(list(render.glob("frame_*.png"))) if render.is_dir() else None
ffprobe = shutil.which("ffprobe") or shutil.which("ffprobe.exe")
info = json.loads(subprocess.run([ffprobe, "-v", "error", "-count_frames", "-show_streams", "-show_format", "-of", "json",
                                  str(video)], capture_output=True, text=True, check=True).stdout)
v = [s for s in info["streams"] if s["codec_type"] == "video"][0]
a = [s for s in info["streams"] if s["codec_type"] == "audio"]
frames, dur = int(v["nb_read_frames"]), float(info["format"]["duration"])
ok = (frames == count - hold and v["width"] == 1920 and v["height"] == 1080 and abs(dur - frames / 30.0) < 0.2
      and len(a) == 1 and pngs in (None, count))
print(json.dumps({"take": take, "lane": lane, "render": str(render), "pngs": pngs, "frame_count": count, "hold": hold,
                  "video_frames": frames, "duration_s": round(dur, 3), "size": f'{v["width"]}x{v["height"]}',
                  "audio": len(a), "video_mb": round(video.stat().st_size / 2**20), "ok": ok}))
if ok and "--delete" in sys.argv and render.is_dir():
    shutil.rmtree(render)
    print(json.dumps({"deleted": str(render), "free_gb": round(shutil.disk_usage(root.anchor).free / 2**30, 1)}))
