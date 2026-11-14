"""Adapter for HF `markov-ai/gaming-500-hours` -> contract-format chunks.

Schema (measured in model_research/from_hakkolite/model/14-input-capture-datasets-deepdive.md §2.2):

    {game}/{workflow_id}/
      clip.mp4            30fps CFR H.264
      events.json         raw NDJSON input events (absolute mouse coords)
      frame_events.json   NDJSON, one line per video frame:
                          {"frame": N, "video_t_ms": T, "events": [...]}
      metadata.json       workflow_id/game/platform/duration/...

COMPLIANCE: the dataset declares no license and records third-party games, so
every converted chunk is written with first_party=false, consent_id=null,
is_trainable=false and tag "method_validation_only". The loader will refuse it
unless data.allow_method_validation_only=true. Never feed this to a product model.

Known caveat (research doc §4 "鼠标绝对坐标陷阱"): mouse is ABSOLUTE screen
coords, so deltas are reconstructed by differencing. Pointer-lock games recenter
the cursor, which corrupts deltas; sessions where one position dominates the
move stream are tagged "pointer_delta:suspect_pointer_lock".

Usage:
    python -m diogenes_phase0.data.gaming500 fetch   --out ml/data/gaming500_raw \
        --games valorant minecraft --max-sessions 2 --events-only
    python -m diogenes_phase0.data.gaming500 convert --raw ml/data/gaming500_raw \
        --out ml/data/gaming500 [--events-only] [--res 192 108] [--max-minutes 10]
"""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
from collections import Counter
from functools import lru_cache
from pathlib import Path

import numpy as np

from ..keys import (
    GAMING500_LEGACY_KEY_SCHEMA,
    foveated_bin,
    map_keycode,
    map_mouse_button,
)

REPO_ID = "markov-ai/gaming-500-hours"
KEY_SCHEMA = GAMING500_LEGACY_KEY_SCHEMA


# ---------------------------------------------------------------------------
# Event-stream -> per-frame ActionLabel (pure, unit-tested offline)
# ---------------------------------------------------------------------------

def frame_events_to_actions(lines: list[dict], platform: str = "windows") -> tuple[list[dict], dict]:
    """Convert frame_events.json rows to contract p0.jsonl rows.

    Returns (action_rows, quality_stats).
    """
    held: set[int] = set()
    last_pos: tuple[float, float] | None = None
    move_positions: Counter = Counter()
    n_moves = 0
    rows: list[dict] = []

    for line in lines:
        dx = dy = 0.0
        for ev in line.get("events", []):
            et = ev.get("type")
            if et == "key":
                vk = map_keycode(ev.get("keyCode"), ev.get("characters"), platform)
                if vk is None:
                    continue
                if ev.get("isDown"):
                    held.add(vk)
                else:
                    held.discard(vk)
            elif et == "click":
                vk = map_mouse_button(ev.get("button", ""))
                if vk is None:
                    continue
                if ev.get("isDown"):
                    held.add(vk)
                else:
                    held.discard(vk)
                if "x" in ev and "y" in ev:
                    last_pos = (float(ev["x"]), float(ev["y"]))
            elif et in ("mouse_move", "drag"):
                if "x" not in ev or "y" not in ev:
                    continue
                pos = (float(ev["x"]), float(ev["y"]))
                if last_pos is not None:
                    dx += pos[0] - last_pos[0]
                    dy += pos[1] - last_pos[1]
                last_pos = pos
                move_positions[(round(pos[0]), round(pos[1]))] += 1
                n_moves += 1
        rows.append({
            "frame": int(line["frame"]),
            "video_t_ms": int(line.get("video_t_ms", 0)),
            "keys": sorted(held),
            "pointerDx": dx, "pointerDy": dy,
            "pointerBins": [foveated_bin(dx), foveated_bin(dy)],
            "src": "gaming500",
        })

    top_frac = (move_positions.most_common(1)[0][1] / n_moves) if n_moves else 0.0
    quality = {
        "n_moves": n_moves,
        "top_position_fraction": round(top_frac, 4),
        "suspect_pointer_lock": bool(n_moves >= 100 and top_frac > 0.2),
    }
    return rows, quality


# ---------------------------------------------------------------------------
# fetch
# ---------------------------------------------------------------------------

def _list_sessions() -> list[dict]:
    from huggingface_hub import HfApi
    entries = HfApi().list_repo_tree(REPO_ID, repo_type="dataset", recursive=True)
    sessions: dict[str, dict] = {}
    for e in entries:
        parts = e.path.split("/")
        if len(parts) == 3:
            game_dir, wid, name = parts
            s = sessions.setdefault(f"{game_dir}/{wid}", {
                "game_dir": game_dir, "workflow_id": wid, "files": [],
                "sizes": {}})
            s["files"].append(name)
            if getattr(e, "size", None):
                s["sizes"][name] = e.size
    return sorted(sessions.values(), key=lambda s: (s["game_dir"], s["workflow_id"]))


def fetch(out: Path, games: list[str] | None, max_sessions: int,
          events_only: bool, per_game: int | None = None,
          max_clip_mb: float | None = None) -> None:
    from huggingface_hub import hf_hub_download
    sessions = _list_sessions()
    if games:
        wanted = [g.lower() for g in games]
        sessions = [s for s in sessions
                    if any(w in s["game_dir"].lower() for w in wanted)]
    if max_clip_mb:
        sessions = [s for s in sessions
                    if s["sizes"].get("clip.mp4", 0) <= max_clip_mb * 1e6]
    # smallest clips first so a size cap favors short sessions
    sessions.sort(key=lambda s: (s["game_dir"], s["sizes"].get("clip.mp4", 0)))
    if per_game:
        kept, seen = [], {}
        for s in sessions:
            if seen.get(s["game_dir"], 0) < per_game:
                kept.append(s)
                seen[s["game_dir"]] = seen.get(s["game_dir"], 0) + 1
        sessions = kept
    sessions = sessions[:max_sessions]
    if not sessions:
        raise SystemExit("no sessions matched the filters")
    names = ["metadata.json", "frame_events.json"] + ([] if events_only else ["clip.mp4"])
    for s in sessions:
        dest = out / s["game_dir"] / s["workflow_id"]
        dest.mkdir(parents=True, exist_ok=True)
        for name in names:
            if name not in s["files"]:
                print(f"  ! {s['game_dir']}/{s['workflow_id']} missing {name}, skipping file")
                continue
            path = hf_hub_download(
                REPO_ID, f"{s['game_dir']}/{s['workflow_id']}/{name}",
                repo_type="dataset")
            data = Path(path).read_bytes()
            (dest / name).write_bytes(data)
            print(f"  fetched {s['game_dir']}/{s['workflow_id']}/{name} ({len(data)/1e6:.1f} MB)")
    print(f"done: {len(sessions)} session(s) under {out}")


# ---------------------------------------------------------------------------
# convert
# ---------------------------------------------------------------------------

def _write_frames(cdir: Path, frames: np.ndarray, codec: str, fps: float) -> None:
    """Persist a chunk's frames as compressed arrays or as video.

    npz is lossless and was fine at method-validation scale, but it does not
    scale: 120 frames at 224x224 come to ~9 MB, so a single game's 4.6 usable
    hours landed at 71 GB — more than a training VM's disk, before any attempt
    to reach the 50-100 h knee the plan targets. H.264 at CRF 18 is ~30x
    smaller and is the same lossy-video regime VPT trained its IDM on.
    """
    if codec == "npz":
        np.savez_compressed(cdir / "p0.npz", frames=frames)
        return
    import cv2
    n, h, w, _ = frames.shape
    if _have_ffmpeg():
        proc = subprocess.Popen(
            ["ffmpeg", "-y", "-loglevel", "error", "-f", "rawvideo",
             "-pix_fmt", "rgb24", "-s", f"{w}x{h}", "-r", str(fps), "-i", "-",
             "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
             "-pix_fmt", "yuv420p", str(cdir / "p0.mp4")],
            stdin=subprocess.PIPE)
        proc.communicate(frames.astype(np.uint8).tobytes())
        if proc.returncode != 0:
            raise RuntimeError(f"ffmpeg failed writing {cdir / 'p0.mp4'}")
        return
    # fallback: OpenCV's writer. mp4v is a weaker codec with no quality knob,
    # so the codec actually used is recorded in meta rather than assumed.
    vw = cv2.VideoWriter(str(cdir / "p0.mp4"),
                         cv2.VideoWriter_fourcc(*"mp4v"), fps, (w, h))
    if not vw.isOpened():
        raise RuntimeError(f"cannot open a video writer for {cdir / 'p0.mp4'}")
    for f in frames:
        vw.write(cv2.cvtColor(f, cv2.COLOR_RGB2BGR))
    vw.release()


@lru_cache(maxsize=1)
def _have_ffmpeg() -> bool:
    return shutil.which("ffmpeg") is not None


def _codec_name(codec: str) -> str:
    if codec == "npz":
        return "npz"
    return "h264" if _have_ffmpeg() else "mp4v"


def _chunk_meta(game: str, session: str, chunk_idx: int, n_frames: int,
                width: int, height: int, fps: float, tags: list[str],
                has_video: bool, codec: str = "npz") -> dict:
    return {
        "sample_key": f"{session}_c{chunk_idx:04d}",
        "session": session,
        "chunk": chunk_idx,
        "game": game,
        "category": "gaming",
        "platform": "pc",
        "video": {"file": (f"p0.{'npz' if codec == 'npz' else 'mp4'}"
                           if has_video else None),
                  "codec": _codec_name(codec),
                  "width": width, "height": height, "fps": fps,
                  "frames": n_frames, "audio": False},
        "annotation_source": "gaming500_frame_events",
        "key_schema": KEY_SCHEMA,
        "rights": "third_party",
        "first_party": False,
        "consent_id": None,
        "is_trainable": False,
        "is_cutscene": False,
        "tags": tags,
    }


def convert_session(session_dir: Path, out_root: Path, *, res: tuple[int, int],
                    chunk_frames: int, max_frames: int | None,
                    events_only: bool, codec: str = "npz") -> int:
    meta = json.loads((session_dir / "metadata.json").read_text())
    game = str(meta.get("game", session_dir.parent.name))
    platform = str(meta.get("platform", "windows"))
    session = f"{session_dir.parent.name}_{session_dir.name[:8]}"

    lines = [json.loads(x) for x in
             (session_dir / "frame_events.json").read_text().splitlines() if x.strip()]
    rows, quality = frame_events_to_actions(lines, platform)
    if max_frames:
        rows = rows[:max_frames]

    tags = ["method_validation_only", "license:undeclared"]
    if quality["suspect_pointer_lock"]:
        tags.append("pointer_delta:suspect_pointer_lock")

    video_iter = None
    width, height = res
    if not events_only:
        import cv2
        cap = cv2.VideoCapture(str(session_dir / "clip.mp4"))
        if not cap.isOpened():
            raise RuntimeError(f"cannot open {session_dir / 'clip.mp4'}")

        def _frames():
            ok, frame = cap.read()
            while ok:
                frame = cv2.resize(frame, (width, height), interpolation=cv2.INTER_AREA)
                yield cv2.cvtColor(frame, cv2.COLOR_BGR2RGB)
                ok, frame = cap.read()
            cap.release()

        video_iter = _frames()

    sess_dir = out_root / game.lower().replace(" ", "_") / session
    n_chunks = 0
    for ci in range(0, len(rows), chunk_frames):
        chunk_rows = rows[ci:ci + chunk_frames]
        if len(chunk_rows) < chunk_frames:
            break  # drop the ragged tail
        frames = None
        if video_iter is not None:
            buf = []
            for _ in range(chunk_frames):
                f = next(video_iter, None)
                if f is None:
                    break
                buf.append(f)
            if len(buf) < chunk_frames:
                break  # video shorter than events; stop at aligned prefix
            frames = np.stack(buf)
        cdir = sess_dir / f"c{n_chunks:04d}"
        cdir.mkdir(parents=True, exist_ok=True)
        if frames is not None:
            _write_frames(cdir, frames, codec, 30.0)
        with open(cdir / "p0.jsonl", "w") as f:
            for j, r in enumerate(chunk_rows):
                f.write(json.dumps({**r, "frame": j, "gframe": ci + j}) + "\n")
        (cdir / "meta.json").write_text(json.dumps(_chunk_meta(
            game, session, n_chunks, len(chunk_rows), width, height,
            30.0, tags, frames is not None, codec), indent=2))
        n_chunks += 1

    (sess_dir / "metadata.json").write_text(json.dumps({
        "workflow_id": meta.get("workflow_id", session_dir.name),
        "game": game, "platform": platform,
        "source": f"hf:{REPO_ID}",
        "pointer_delta_quality": quality,
        "rights": "third_party", "first_party": False,
        "consent_id": None, "is_trainable": False, "tags": tags,
    }, indent=2))
    return n_chunks


def convert(raw: Path, out: Path, *, res: tuple[int, int], chunk_frames: int,
            max_minutes: float | None, events_only: bool,
            codec: str = "npz") -> None:
    max_frames = int(max_minutes * 60 * 30) if max_minutes else None
    session_dirs = sorted(p.parent for p in raw.rglob("frame_events.json"))
    if not session_dirs:
        raise SystemExit(f"no sessions with frame_events.json under {raw}")
    total = 0
    for sd in session_dirs:
        n = convert_session(sd, out, res=res, chunk_frames=chunk_frames,
                            max_frames=max_frames, events_only=events_only,
                            codec=codec)
        print(f"  {sd.parent.name}/{sd.name}: {n} chunk(s)")
        total += n
    print(f"done: {total} chunk(s) under {out} (method_validation_only)")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    pf = sub.add_parser("fetch")
    pf.add_argument("--out", required=True, type=Path)
    pf.add_argument("--games", nargs="*", default=None,
                    help="substring filter on the game directory name")
    pf.add_argument("--max-sessions", type=int, default=2)
    pf.add_argument("--per-game", type=int, default=None,
                    help="cap sessions per game dir (smallest clips first)")
    pf.add_argument("--max-clip-mb", type=float, default=None,
                    help="skip sessions whose clip.mp4 exceeds this size")
    pf.add_argument("--events-only", action="store_true",
                    help="skip clip.mp4 (label-side validation only)")

    pc = sub.add_parser("convert")
    pc.add_argument("--raw", required=True, type=Path)
    pc.add_argument("--out", required=True, type=Path)
    pc.add_argument("--res", nargs=2, type=int, default=[192, 108],
                    metavar=("W", "H"))
    pc.add_argument("--chunk-frames", type=int, default=120)
    pc.add_argument("--max-minutes", type=float, default=10.0,
                    help="cap converted footage per session (0 = no cap)")
    pc.add_argument("--events-only", action="store_true")
    pc.add_argument("--codec", choices=["npz", "mp4"], default="npz",
                    help="frame storage: npz is lossless but ~30x larger, "
                         "which caps a run at a few usable hours per disk")

    args = p.parse_args()
    if args.cmd == "fetch":
        fetch(args.out, args.games, args.max_sessions, args.events_only,
              per_game=args.per_game, max_clip_mb=args.max_clip_mb)
    else:
        convert(args.raw, args.out, res=tuple(args.res),
                chunk_frames=args.chunk_frames,
                max_minutes=args.max_minutes or None,
                events_only=args.events_only, codec=args.codec)


if __name__ == "__main__":
    main()
