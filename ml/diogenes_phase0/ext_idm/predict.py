#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "loguru",
#     "torch>=2.8.0",
#     "torchvision",
#     "transformers>=5.0.0",
#     "accelerate",
#     "mcap-owa-support>=0.6.6",
#     "owa-core>=0.6.6",
#     "owa-msgs>=0.6.6",
#     "owa-env-desktop>=0.6.6",
#     "owa-data @ git+https://github.com/open-world-agents/open-world-agents@f1dcd4428006dc65ee2cc10da497b6b139113221#subdirectory=projects/owa-data",
# ]
#
# [tool.uv]
# exclude-newer = "2026-07-16"
# ///
"""Run Generalist-IDM-1B over contract chunk dirs; dump its predicted events.

    uv run --script predict.py --chunks chunks.txt --out preds/ --d2e-repo ~/D2E

The dependency block above is **copied verbatim from D2E/inference.py** and
must stay that way: this script imports that file and runs its
`InferencePipeline` rather than reimplementing it, so that a number reported
here is the upstream model under the upstream decoding loop, not our reading
of it. `uv run --script` resolves the block into its own environment, which is
also why this file must never be imported from the training venv — owa-* and
our torch pin do not have to agree.

Three choices this script makes, all of which change the numbers:

1. **Every chunk is re-encoded to 448x448 @ 60 fps first**, by calling the
   upstream `preprocess_video`. That is the geometry the model was trained on,
   and it makes the 20 Hz screen grid land on exact frame boundaries instead of
   halfway between two 30 fps frames. Applied to BOTH resolution conditions, so
   `data/d2e30` (224 source, upscaled here) and `data/d2e448` (480p source)
   differ only in how much detail survived our converter — which is the point
   of running both.
2. **No time shift is applied here.** Upstream shifts predicted action
   timestamps 100 ms earlier at output (`--time-shift`, default 0.1) because
   the model emits an action after observing its effect. Applying it during
   inference would bake one alignment into the artifact; we record raw
   timestamps and let `score.py` sweep the shift, the same "try both
   alignments, take the best" convention doc 26 §2.5 adopted for `flow_signal`
   after a one-frame offset silently ate part of a correlation.
3. **Each chunk is an independent 4-second episode**, with no context carried
   across chunk boundaries. Chunks are already the unit our own eval scores, so
   this keeps both models on the same observation budget — but it does mean a
   key held down since before the chunk started has no press event anywhere in
   the model's input. `score.py` handles that explicitly (see its `--seed`),
   because silently reading "no press event" as "not pressed" would score W —
   the most-held key in these recordings — against a state the model was never
   shown.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def _load_upstream(d2e_repo: Path):
    """Import the upstream inference.py as a module, without installing it."""
    path = d2e_repo / "inference.py"
    if not path.exists():
        raise SystemExit(
            f"{path} not found — clone it first:\n"
            f"  git clone https://github.com/worv-ai/D2E.git {d2e_repo}")
    spec = importlib.util.spec_from_file_location("d2e_inference", path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["d2e_inference"] = mod
    spec.loader.exec_module(mod)
    return mod


def _event_payload(decoded) -> dict:
    """Message body -> plain dict, tolerant of the owa-msgs schema version.

    `timestamp` and `topic` are dropped: the body carries its own `timestamp`
    field and it is None on a generated event, while the one that matters lives
    on the McapMessage. Letting the body win cost a smoke run — every event
    came back with `timestamp: None` and nothing raised.
    """
    drop = {"timestamp", "topic"}
    for attr in ("model_dump", "dict"):
        fn = getattr(decoded, attr, None)
        if callable(fn):
            try:
                return {k: v for k, v in fn().items()
                        if k not in drop
                        and isinstance(v, (int, float, str, bool, type(None)))}
            except Exception:  # noqa: BLE001 — fall through to field probing
                pass
    out = {}
    for k in ("event_type", "vk", "last_x", "last_y", "button_flags",
              "button_data", "dx", "dy"):
        v = getattr(decoded, k, None)
        if isinstance(v, (int, float, str, bool)):
            out[k] = v
    return out


def write_screen_mcap(video: Path, mcap_path: Path, duration_s: float,
                      screen_hz: float) -> int:
    """Screen-only input MCAP, one message per grid tick — as upstream does.

    Upstream derives the tick count from ffprobe's duration; we take it from
    the chunk's frame count instead. Same construction, but a chunk whose
    container rounds 4.000 s to 3.983 s keeps its 80th observation.
    """
    from mcap_owa.highlevel import OWAMcapWriter
    from owa.core import MESSAGES

    ScreenCaptured = MESSAGES["desktop/ScreenCaptured"]
    uri = str(video.resolve())
    interval_ns = int(1e9 / screen_hz)
    n = int(duration_s * screen_hz)
    with OWAMcapWriter(str(mcap_path)) as w:
        for i in range(n):
            ts = i * interval_ns
            w.write_message(
                ScreenCaptured(utc_ns=ts, media_ref={"uri": uri, "pts_ns": ts}),
                topic="screen", timestamp=ts)
    return n


def predict_chunk(pipeline, up, chunk: Path, tmpdir: Path, *, fps: float,
                  screen_hz: float, action_topics: set[str]) -> dict:
    n_frames = sum(1 for line in open(chunk / "p0.jsonl") if line.strip())
    duration_s = n_frames / fps

    src = chunk / "p0.mp4"
    if not src.exists():
        raise SystemExit(f"{chunk} has no p0.mp4 (npz chunks are not supported)")
    prepped = tmpdir / "prepped.mkv"
    up.preprocess_video(str(src), str(prepped), duration_s)

    input_mcap = tmpdir / "input.mcap"
    n_screen = write_screen_mcap(prepped, input_mcap, duration_s, screen_hz)

    from mcap_owa.highlevel import OWAMcapReader

    events: list[dict] = []

    def collect(msg):
        if msg.topic in action_topics:
            # explicit keys LAST: the body must never overwrite them
            events.append({**_event_payload(msg.decoded),
                           "topic": msg.topic, "timestamp": int(msg.timestamp)})

    def screens():
        with OWAMcapReader(str(input_mcap)) as reader:
            for msg in reader.iter_messages(topics=["screen"]):
                msg.decoded.resolve_relative_path(str(input_mcap))
                yield msg

    t0 = time.time()
    pipeline.run_generation(screens(), collect, apply_resampler=True)
    return {
        "chunk": str(chunk), "n_frames": n_frames, "fps": fps,
        "duration_s": duration_s, "n_screen_events": n_screen,
        "screen_hz": screen_hz, "wall_s": round(time.time() - t0, 2),
        "events": events,
    }


def out_name(chunk: Path) -> str:
    """`<game>__<session>__<chunk>.json` — flat, and collision-free."""
    return f"{chunk.parent.parent.name}__{chunk.parent.name}__{chunk.name}.json"


# ---------------------------------------------------------------------------
# continuous decoding (research/30 D-3-b)
# ---------------------------------------------------------------------------


def _first_gframe(chunk: Path) -> int:
    with open(chunk / "p0.jsonl") as f:
        return int(json.loads(f.readline())["gframe"])


def _n_frames(chunk: Path) -> int:
    return sum(1 for line in open(chunk / "p0.jsonl") if line.strip())


def runs_from_chunks(chunks: list[Path]) -> list[list[tuple[Path, int, int]]]:
    """Group chunks into maximal runs that are contiguous on the resampled grid.

    `gframe` is the converter's grid tick, so `last + 1 == next.first` is the
    only honest contiguity test — chunk DIRECTORY numbering skips the chunks
    the stall detector dropped, and c0007 following c0006 does not imply the
    footage is continuous. Concatenating across such a gap would splice two
    unrelated moments into one episode and hand the model a discontinuity we
    then blamed on the model.
    """
    items = [(c, _first_gframe(c), _n_frames(c)) for c in chunks]
    items.sort(key=lambda x: (str(x[0].parent), x[1]))
    runs: list[list[tuple[Path, int, int]]] = []
    cur: list[tuple[Path, int, int]] = []
    for it in items:
        if cur and it[0].parent == cur[-1][0].parent \
                and it[1] == cur[-1][1] + cur[-1][2]:
            cur.append(it)
        else:
            if cur:
                runs.append(cur)
            cur = [it]
    if cur:
        runs.append(cur)
    return runs


def run_out_name(run: list[tuple[Path, int, int]]) -> str:
    first, last = run[0][0], run[-1][0]
    return (f"run__{first.parent.parent.name}__{first.parent.name}__"
            f"{first.name}-{last.name}.json")


def concat_chunk_videos(run, tmpdir: Path) -> Path:
    """Splice the run's chunk mp4s back into one continuous video."""
    listing = tmpdir / "concat.txt"
    listing.write_text("".join(
        f"file '{(c / 'p0.mp4').resolve()}'\n" for c, _, _ in run))
    joined = tmpdir / "joined.mp4"
    cmd = ["ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(listing),
           "-c", "copy", str(joined)]
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"ffmpeg concat failed: {r.stderr[-2000:]}")
    # the frame count is load-bearing: a silently short splice would shift every
    # label after the gap, and nothing downstream would notice
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", "v:0", "-count_frames",
         "-show_entries", "stream=nb_read_frames", "-of",
         "default=noprint_wrappers=1:nokey=1", str(joined)],
        capture_output=True, text=True)
    got = int(probe.stdout.strip())
    want = sum(n for _, _, n in run)
    if got != want:
        raise RuntimeError(
            f"concat produced {got} frames, expected {want} — refusing to "
            f"score a run whose frames do not line up with its labels")
    return joined


def predict_run(pipeline, up, run, tmpdir: Path, *, fps: float,
                screen_hz: float, action_topics: set[str]) -> dict:
    """Decode a whole run as ONE episode; events stay on the run's timeline."""
    total = sum(n for _, _, n in run)
    duration_s = total / fps
    joined = concat_chunk_videos(run, tmpdir)
    prepped = tmpdir / "prepped.mkv"
    up.preprocess_video(str(joined), str(prepped), duration_s)

    input_mcap = tmpdir / "input.mcap"
    n_screen = write_screen_mcap(prepped, input_mcap, duration_s, screen_hz)

    from mcap_owa.highlevel import OWAMcapReader

    events: list[dict] = []

    def collect(msg):
        if msg.topic in action_topics:
            events.append({**_event_payload(msg.decoded),
                           "topic": msg.topic, "timestamp": int(msg.timestamp)})

    def screens():
        with OWAMcapReader(str(input_mcap)) as reader:
            for msg in reader.iter_messages(topics=["screen"]):
                msg.decoded.resolve_relative_path(str(input_mcap))
                yield msg

    t0 = time.time()
    pipeline.run_generation(screens(), collect, apply_resampler=True)

    members, off = [], 0
    for c, g0, n in run:
        members.append({"chunk": str(c), "n_frames": n, "offset": off,
                        "gframe0": g0})
        off += n
    return {
        "run_id": run_out_name(run)[:-5], "members": members,
        "n_frames_total": total, "fps": fps, "duration_s": duration_s,
        "n_screen_events": n_screen, "screen_hz": screen_hz,
        "wall_s": round(time.time() - t0, 2), "events": events,
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--chunks", required=True, type=Path,
                    help="text file, one chunk dir per line")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--d2e-repo", type=Path, default=Path.home() / "D2E")
    ap.add_argument("--model", default="open-world-agents/Generalist-IDM-1B")
    ap.add_argument("--device", default="cuda")
    ap.add_argument("--max-context-length", type=int, default=2048)
    ap.add_argument("--screen-hz", type=float, default=20.0)
    ap.add_argument("--fps", type=float, default=30.0,
                    help="the chunk grid's fps; sets each chunk's duration")
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--concat", action="store_true",
                    help="decode each contiguous run of chunks as ONE episode "
                         "(research/30 D-3-b) instead of one episode per chunk")
    args = ap.parse_args()

    up = _load_upstream(args.d2e_repo)
    chunks = [Path(l.strip()) for l in args.chunks.read_text().splitlines()
              if l.strip() and not l.startswith("#")]
    if args.limit:
        chunks = chunks[:args.limit]
    args.out.mkdir(parents=True, exist_ok=True)

    cfg = up.InferenceConfig(
        model_path=args.model, device=args.device,
        max_context_length=args.max_context_length,
        # Raw timestamps: score.py sweeps the shift. See module docstring (2).
        time_shift_seconds_for_action=None,
    )
    action_topics = set(cfg.action_topics)
    pipeline = up.InferencePipeline(cfg)
    (args.out / "_run.json").write_text(json.dumps({
        "model": args.model, "max_context_length": args.max_context_length,
        "screen_hz": args.screen_hz, "fps": args.fps,
        "time_shift_applied": None, "action_topics": sorted(action_topics),
        "n_chunks_requested": len(chunks),
        "episode_mode": "run" if args.concat else "chunk",
    }, indent=2))

    if args.concat:
        runs = runs_from_chunks(chunks)
        print(f"{len(chunks)} chunk(s) -> {len(runs)} contiguous run(s): "
              + ", ".join(f"{len(r)}x" for r in runs), flush=True)
        t_start = time.time()
        for i, run in enumerate(runs):
            dst = args.out / run_out_name(run)
            if dst.exists():
                continue
            with tempfile.TemporaryDirectory() as td:
                rec = predict_run(pipeline, up, run, Path(td), fps=args.fps,
                                  screen_hz=args.screen_hz,
                                  action_topics=action_topics)
            dst.write_text(json.dumps(rec))
            print(f"[run {i + 1}/{len(runs)}] {rec['run_id']}: "
                  f"{len(rec['members'])} chunk(s), {len(rec['events'])} events "
                  f"in {rec['wall_s']}s (x{rec['wall_s'] / rec['duration_s']:.1f} "
                  f"realtime)  elapsed {(time.time() - t_start) / 60:.1f}m",
                  flush=True)
        print(f"done: {len(runs)} run(s) under {args.out}", flush=True)
        return

    done = 0
    t_start = time.time()
    for i, chunk in enumerate(chunks):
        dst = args.out / out_name(chunk)
        if dst.exists():
            done += 1
            continue
        with tempfile.TemporaryDirectory() as td:
            rec = predict_chunk(pipeline, up, chunk, Path(td), fps=args.fps,
                                screen_hz=args.screen_hz,
                                action_topics=action_topics)
        dst.write_text(json.dumps(rec))
        done += 1
        el = time.time() - t_start
        print(f"[{i + 1}/{len(chunks)}] {chunk.parent.parent.name}/"
              f"{chunk.parent.name}/{chunk.name}: {len(rec['events'])} events "
              f"in {rec['wall_s']}s  (x{rec['wall_s'] / rec['duration_s']:.1f} "
              f"realtime)  elapsed {el / 60:.1f}m", flush=True)
    print(f"done: {done} chunk(s) under {args.out}", flush=True)


if __name__ == "__main__":
    main()
