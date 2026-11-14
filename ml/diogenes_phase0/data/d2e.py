"""D2E (OWAMcap) → chunk contract adapter.

    python -m diogenes_phase0.data.d2e convert --raw data/d2e_raw --out data/d2e

D2E-480p is the only open corpus whose action fields line up with the
DataMiner contract: VK keyboard codes, **raw HID pointer deltas** rather than
absolute cursor positions, ns timestamps, and per-frame presentation stamps
that tie the event stream to the video. Two things the IDM line could not do
without it:

- the mouse magnitude question. gaming-500 differences absolute cursor
  positions, a lossy proxy under pointer lock, and our own first-party corpus
  is 2.5 h with zero view-scale calibration. D2E carries `mouse/raw.last_x/y`
  straight from the HID report.
- the held-out-game zero-shot criterion, which is the stop-loss condition in
  research/01 §6. It needs several games in one contract; D2E has 30.

Compliance: upstream is CC BY-NC 4.0, so every chunk is stamped
`is_trainable=false` + `method_validation_only` and the loader refuses it
without `allow_method_validation_only=true`. **Never a product training
corpus.** Cite arXiv:2510.05684 for any result built on it.

Two conversion decisions this module makes explicitly, because a default
would silently define the labels (the mirror's DATASHEET flags both):

1. **fps is measured per recording, never assumed — and D2E is variable
   rate.** Across 13 FPS-genre recordings no two agree and one Counter-Strike 2
   recording sits far below the rest; the upstream "480p60" label is nominal.
   (An earlier survey put the spread at 21.35-59.38 fps using the median gap;
   `measured_fps` now reports delivered frames per second, which is lower
   wherever a recording stalls.) So the true rate goes into `video.fps` per chunk, and the
   mixed-fps hard gate (docs/50 P-4) refuses to train across them — correctly,
   because a 5-frame window spans 85 ms at 59 fps and 234 ms at 21 fps.
   `--target-fps` normalises onto a fixed wall-clock grid: each output frame
   takes the newest source frame at or before its timestamp and sums the raw
   deltas over its interval. It **refuses to upsample** — if two output frames
   would land on the same source frame, the extra ones would be duplicates
   presented as observations — but the refusal is per chunk, not per
   recording: the chunks such ticks land in are dropped (and counted in the
   session metadata), and only a recording with no clean chunk left is
   skipped. Integer frame grouping cannot do this job (53.88/2 = 26.94,
   not 30), which is why it was replaced.
2. **A frame's action is the input in `(t_prev, t_frame]`** — what produced
   this frame, not what follows it. The alternative shifts every label by one
   frame; both are defensible, so the choice is recorded here rather than
   left to reading the code.
"""
from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path

import numpy as np

from ..keys import (
    DATA_DEMO_LEGACY_KEY_SCHEMA,
    MOUSE_CLAMP,
    SCANNED_VKEYS,
    foveated_bin,
)
from .gaming500 import _codec_name, _write_frames

UPSTREAM = "hf:open-world-agents/D2E-480p"
LICENSE_TAG = "license:cc-by-nc-4.0"

# 指针事件率低于此值的录制视为「没有指针通道」。D2E 有 30 款游戏，其中相当一部分是
# 键盘/回合制的，它们的 mouse/raw 稀疏到以百秒计一条。按 30 fps 算，1 Hz 意味着 96% 以上
# 的帧 delta 恰好为零——把这些零平均进鼠标 R²，那个门量的就是语料的空闲度而不是模型。
# 所以在转换时就打标，让下游能在算 R² 之前排除，而不是让它们悄悄拉高分母。
MIN_POINTER_HZ = 1.0

# RAWMOUSE button transition flags (winuser.h). Only the buttons the contract
# scans are decoded; the rest stay unmapped rather than being guessed into a
# neighbouring VK.
_BTN_FLAGS = [
    (0x0001, 1, True), (0x0002, 1, False),      # left down / up   -> VK 1
    (0x0004, 2, True), (0x0008, 2, False),      # right down / up  -> VK 2
]
_MOUSE_VKS = {1, 2}


def _held_from_state(buttons) -> set[int]:
    """`keyboard/state.buttons` / `mouse/state.buttons` -> VK set.

    Upstream writes either ints or names depending on the recorder version;
    anything unrecognised is dropped rather than mapped by guesswork.
    """
    out: set[int] = set()
    for b in buttons or []:
        if isinstance(b, bool):
            continue
        if isinstance(b, int):
            out.add(b)
        elif isinstance(b, str):
            s = b.strip().lower()
            named = {"left": 1, "button.left": 1, "right": 2, "button.right": 2}
            if s in named:
                out.add(named[s])
            elif s.startswith("vk_") and s[3:].isdigit():
                out.add(int(s[3:]))
            elif s.isdigit():
                out.add(int(s))
    return out


def read_events(mcap_path: Path) -> dict:
    """Parse one OWAMcap file into frame times and input events."""
    from mcap.reader import make_reader

    frames: list[tuple[int, int]] = []          # (log_time, pts_ns)
    raw: list[tuple[int, int, int, int]] = []    # (t, dx, dy, button_flags)
    kbd: list[tuple[int, str, int]] = []         # (t, press|release, vk)
    kstate: list[tuple[int, set[int]]] = []      # (t, held vks)
    mstate: list[tuple[int, set[int]]] = []      # (t, held mouse vks)
    window: list[tuple[int, str]] = []
    video_uri = None

    with open(mcap_path, "rb") as f:
        for _schema, channel, msg in make_reader(f).iter_messages():
            t = msg.log_time
            try:
                body = json.loads(msg.data)
            except Exception:                     # noqa: BLE001
                continue
            topic = channel.topic
            if topic == "screen":
                ref = body.get("media_ref") or {}
                pts = ref.get("pts_ns")
                if pts is None:
                    continue
                frames.append((t, int(pts)))
                video_uri = video_uri or ref.get("uri")
            elif topic == "mouse/raw":
                raw.append((t, int(body.get("last_x", 0)),
                            int(body.get("last_y", 0)),
                            int(body.get("button_flags", 0))))
            elif topic == "keyboard":
                vk = body.get("vk")
                if vk is not None:
                    kbd.append((t, str(body.get("event_type", "")), int(vk)))
            elif topic == "keyboard/state":
                kstate.append((t, _held_from_state(body.get("buttons"))))
            elif topic == "mouse/state":
                mstate.append((t, _held_from_state(body.get("buttons"))))
            elif topic == "window":
                window.append((t, str(body.get("title", ""))))

    frames.sort(key=lambda x: x[1])
    for lst in (raw, kbd, kstate, mstate, window):
        lst.sort(key=lambda x: x[0])
    return {"frames": frames, "raw": raw, "kbd": kbd, "kstate": kstate,
            "mstate": mstate, "window": window, "video_uri": video_uri}


def measured_fps(frames: list[tuple[int, int]]) -> float:
    """Frames actually delivered per second, over the whole recording.

    The sustained average `(n-1) / span`, not the median inter-frame gap. On a
    steady source the two agree; they diverge exactly when the recording drops
    frames, and then the median flatters it — one D2E sample measures 64.24 fps
    by median while delivering 60.02. `video.fps` feeds the mixed-fps gate and
    the window's wall-clock length, so the honest number is what was delivered.
    `frame_gap_median_fps` keeps the median as a diagnostic beside it.
    """
    if len(frames) < 3:
        raise ValueError("need at least three frames to measure fps")
    pts = [p for _, p in frames]
    span = pts[-1] - pts[0]
    if span <= 0:
        raise ValueError("non-monotonic frame timestamps")
    return 1e9 * (len(pts) - 1) / span


def median_gap_fps(frames: list[tuple[int, int]]) -> float:
    """Rate implied by the median inter-frame gap — diagnostic only.

    Its gap from `measured_fps` is a drop-rate signal: equal means steady,
    higher means the recording stalled somewhere.
    """
    d = np.diff([p for _, p in frames])
    med = float(np.median(d))
    return 1e9 / med if med > 0 else 0.0


def plan_resample(ev: dict, target_fps: float) -> tuple[list[int], list[int]]:
    """Output timestamps on a fixed grid, and the source frame each one uses.

    D2E is variable-rate: across 13 recordings the measured rate spans
    21.35–59.38 fps (one Counter-Strike 2 recording is 21.35). Integer frame
    grouping cannot normalise that — 53.88/2 is 26.94, not 30 — so the grid has
    to be built in wall-clock time and each output frame takes the newest
    source frame at or before its timestamp.

    A tick that lands on an already-used source frame (idx repeats) marks a
    stretch the recording did not actually cover — either a stall's dropped
    frames or a target rate above what the recording delivers. The tick is
    returned as-is: `convert_pair` drops the chunks such ticks land in, so a
    repeated frame is never presented as an observation, and refuses the
    recording only when no clean chunk survives. Vetoing the whole recording
    here would let a single dropped frame discard an hour of clean footage —
    that is how 12 of 13 D2E recordings (Rainbow Six entirely, one VALORANT
    recording over 3 frames) vanished from the first D-0 run.
    """
    pts = [p for _, p in ev["frames"]]
    if len(pts) < 3:
        raise SystemExit("need at least three frames to resample")
    step = 1e9 / target_fps
    out_times, idx = [], []
    t, last = float(pts[0]), pts[-1]
    j = 0
    while t <= last:
        while j + 1 < len(pts) and pts[j + 1] <= t:
            j += 1
        out_times.append(int(t))
        idx.append(j)
        t += step
    return out_times, idx


def build_rows(ev: dict, *, out_times: list[int] | None = None) -> list[dict]:
    """Contract rows, one per output timestamp.

    `out_times` defaults to the source frame timestamps (native rate, no
    resampling). Pass a fixed grid from `plan_resample` to normalise a
    variable-rate recording. Either way a row's action is the input in
    `(t_prev, t]` — what produced that frame.
    """
    frames, raw, kbd = ev["frames"], ev["raw"], ev["kbd"]
    kstate, mstate = ev["kstate"], ev["mstate"]
    if not frames:
        return []

    held: set[int] = set()
    mheld: set[int] = set()
    # Seed from the first periodic snapshot. A `_split_NN` file's opening
    # frames precede its first state message, so without this every key held
    # in from the previous split reads as "not pressed" for up to a second —
    # and W is held almost continuously in these recordings. Only safe when no
    # transition happened before that snapshot, otherwise the snapshot
    # describes a later state than frame 0.
    if kstate and (not kbd or kbd[0][0] >= kstate[0][0]):
        held = set(kstate[0][1])
    if mstate:
        first_btn = next((t for t, _, _, fl in raw if fl), None)
        if first_btn is None or first_btn >= mstate[0][0]:
            mheld = {v for v in mstate[0][1] if v in _MOUSE_VKS}
    ri = ki = si = mi = 0
    prev_t = None
    rows: list[dict] = []
    pending_dx = pending_dy = 0
    times = out_times if out_times is not None else [p for _, p in frames]

    for ft in times:
        lo = -1 if prev_t is None else prev_t
        # keyboard: replay transitions, but let a periodic full state override.
        # D2E files are `_split_NN`; a key held across a split boundary has no
        # press event inside this file, and a missed release would stick for
        # the rest of the recording.
        while si < len(kstate) and kstate[si][0] <= ft:
            held = set(kstate[si][1])
            si += 1
        while ki < len(kbd) and kbd[ki][0] <= ft:
            t, kind, vk = kbd[ki]
            if t > lo or prev_t is None:
                if kind == "press":
                    held.add(vk)
                elif kind == "release":
                    held.discard(vk)
            ki += 1
        while mi < len(mstate) and mstate[mi][0] <= ft:
            mheld = {v for v in mstate[mi][1] if v in _MOUSE_VKS}
            mi += 1

        dx = dy = 0
        while ri < len(raw) and raw[ri][0] <= ft:
            t, rdx, rdy, flags = raw[ri]
            if t > lo or prev_t is None:
                dx += rdx
                dy += rdy
                for bit, vk, down in _BTN_FLAGS:
                    if flags & bit:
                        mheld.add(vk) if down else mheld.discard(vk)
            ri += 1

        pending_dx += dx
        pending_dy += dy
        prev_t = ft

        keys = sorted((held | mheld) & set(SCANNED_VKEYS))
        cdx = float(np.clip(pending_dx, -MOUSE_CLAMP, MOUSE_CLAMP))
        cdy = float(np.clip(pending_dy, -MOUSE_CLAMP, MOUSE_CLAMP))
        rows.append({
            "keys": keys,
            "pointerDx": int(cdx), "pointerDy": int(cdy),
            "pointerRawDx": int(pending_dx), "pointerRawDy": int(pending_dy),
            "pointerBins": [foveated_bin(cdx), foveated_bin(cdy)],
        })
        pending_dx = pending_dy = 0
    return rows


def _frames_iter(mkv: Path, res: tuple[int, int],
                 indices: list[int] | None = None):
    """Decode sequentially, yielding either every frame or the chosen ones.

    Sequential decode with skipping rather than random seeks: seeking in a
    long-GOP H.264 stream lands on the nearest keyframe, so a "seek to frame
    n" would quietly return a different frame than the timestamps chose.
    """
    import cv2

    cap = cv2.VideoCapture(str(mkv))
    try:
        if indices is None:
            while True:
                ok, fr = cap.read()
                if not ok:
                    return
                yield cv2.resize(cv2.cvtColor(fr, cv2.COLOR_BGR2RGB), res)
            return
        pos = -1
        cur = None
        for want in indices:
            while pos < want:
                ok, fr = cap.read()
                if not ok:
                    return
                pos += 1
                cur = fr
            if cur is None:
                return
            yield cv2.resize(cv2.cvtColor(cur, cv2.COLOR_BGR2RGB), res)
    finally:
        cap.release()


def convert_pair(mcap: Path, mkv: Path, out_root: Path, *, game: str,
                 res: tuple[int, int] = (224, 224), chunk_frames: int = 120,
                 codec: str = "mp4", target_fps: float | None = None,
                 max_chunks: int | None = None) -> int:
    ev = read_events(mcap)
    if not ev["frames"]:
        print(f"  {mcap.name}: no screen frames, skipped")
        return 0
    src_fps = measured_fps(ev["frames"])
    pts = [p for _, p in ev["frames"]]
    span_s = max(1e-9, (pts[-1] - pts[0]) / 1e9)
    pointer_hz = len(ev["raw"]) / span_s
    tags = ["method_validation_only", LICENSE_TAG, "source:d2e-480p",
            "pointer:raw_hid"]
    if pointer_hz < MIN_POINTER_HZ:
        tags.append("pointer:absent_or_sparse")
    if target_fps:
        out_times, indices = plan_resample(ev, target_fps)
        out_fps = float(target_fps)
        tags.append(f"resampled:{target_fps:g}fps_from_{src_fps:.2f}")
    else:
        out_times, indices = None, None
        out_fps = src_fps

    rows = build_rows(ev, out_times=out_times)
    session = mcap.stem
    sess_dir = out_root / game.lower().replace(" ", "_") / session
    frames_it = _frames_iter(mkv, res, indices)

    # A repeated source index means the grid tick shows a frame the previous
    # tick already showed: the recording has no fresh observation there. The
    # first presentation (tick k-1) is real; only the repeat poisons its chunk.
    dup_ticks = 0
    dirty: set[int] = set()
    if indices is not None:
        for k in range(1, len(indices)):
            if indices[k] == indices[k - 1]:
                dup_ticks += 1
                dirty.add(k // chunk_frames)

    n_chunks = 0
    n_dropped = 0
    for ci in range(0, len(rows), chunk_frames):
        if max_chunks is not None and n_chunks >= max_chunks:
            break
        chunk_rows = rows[ci:ci + chunk_frames]
        if len(chunk_rows) < chunk_frames:
            break                                  # drop the ragged tail
        buf = []
        for _ in range(chunk_frames):
            f = next(frames_it, None)
            if f is None:
                break
            buf.append(f)
        if len(buf) < chunk_frames:
            break                                  # video shorter than events
        if ci // chunk_frames in dirty:
            n_dropped += 1                         # frames consumed, not written
            continue
        cdir = sess_dir / f"c{n_chunks:04d}"
        cdir.mkdir(parents=True, exist_ok=True)
        _write_frames(cdir, np.stack(buf), codec, out_fps)
        with open(cdir / "p0.jsonl", "w") as f:
            for j, r in enumerate(chunk_rows):
                f.write(json.dumps({**r, "frame": j, "gframe": ci + j}) + "\n")
        (cdir / "meta.json").write_text(json.dumps({
            "sample_key": f"{session}_c{n_chunks:04d}",
            "session": session, "chunk": n_chunks,
            "game": game, "category": "gaming", "platform": "pc",
            "video": {"file": f"p0.{'npz' if codec == 'npz' else 'mp4'}",
                      "codec": _codec_name(codec),
                      "width": res[0], "height": res[1],
                      "fps": out_fps, "frames": len(chunk_rows),
                      "audio": False},
            "annotation_source": "d2e_owamcap",
            "key_schema": DATA_DEMO_LEGACY_KEY_SCHEMA,
            "pointer_delta_quality": "raw_hid",
            "pointer_event_hz": round(pointer_hz, 3),
            "rights": "third_party", "first_party": False,
            "consent_id": None, "is_trainable": False, "is_cutscene": False,
            "tags": tags,
        }, indent=2))
        n_chunks += 1

    if dup_ticks and not n_chunks:
        raise SystemExit(
            f"target {target_fps:g} fps needs {dup_ticks} duplicated frame(s) — "
            f"the recording delivers about {src_fps:.2f} fps. Upsampling would "
            f"present repeated frames as observations, and no clean "
            f"{chunk_frames}-frame chunk survives; pick a lower target.")
    if n_dropped:
        print(f"  {mcap.name}: dropped {n_dropped} stall chunk(s) covering "
              f"{dup_ticks} duplicated grid tick(s), kept {n_chunks}")

    if n_chunks:
        (sess_dir / "metadata.json").write_text(json.dumps({
            "session": session, "game": game, "source": UPSTREAM,
            "source_fps": src_fps,
            "pointer_event_hz": round(pointer_hz, 3),
            "frame_gap_median_fps": median_gap_fps(ev["frames"]),
            "output_fps": out_fps,
            "resampled": bool(target_fps),
            "dup_ticks": dup_ticks,
            "stall_chunks_dropped": n_dropped,
            "pointer_delta_quality": "raw_hid",
            "window_titles": sorted({t for _, t in ev["window"]})[:8],
            "rights": "third_party", "first_party": False,
            "consent_id": None, "is_trainable": False, "tags": tags,
        }, indent=2))
    return n_chunks


def convert(raw: Path, out: Path, **kw) -> dict:
    """Every `<name>.mcap` with a sibling `<name>.mkv`; game = parent dir.

    A recording that cannot reach `target_fps` is **skipped with its reason
    printed**, not fatal: D2E is variable-rate, so on a 30 fps grid the slow
    recordings drop out and the rest still form a single-rate corpus the fps
    gate accepts. Aborting the whole walk on the first slow recording would
    make one bad file veto 30 games. The skipped list is returned so a caller
    can report what a run does not cover — a silently smaller corpus reads as
    a complete one.
    """
    total, skipped, sparse = 0, [], []
    for mcap in sorted(raw.rglob("*.mcap")):
        mkv = mcap.with_suffix(".mkv")
        if not mkv.exists():
            skipped.append((str(mcap.name), "no paired .mkv"))
            print(f"  {mcap.name}: no paired .mkv, skipped")
            continue
        game = mcap.parent.name if mcap.parent != raw else "unknown"
        try:
            n = convert_pair(mcap, mkv, out, game=game, **kw)
        except SystemExit as e:
            skipped.append((f"{game}/{mcap.stem}", str(e)))
            print(f"  {game}/{mcap.stem}: SKIPPED — {e}")
            continue
        print(f"  {game}/{mcap.stem}: {n} chunk(s)")
        if n:
            meta = json.loads(next((out / game.lower().replace(" ", "_") /
                                    mcap.stem).glob("c*/meta.json")).read_text())
            if "pointer:absent_or_sparse" in meta.get("tags", []):
                sparse.append((f"{game}/{mcap.stem}",
                               meta.get("pointer_event_hz")))
        total += n
    print(f"done: {total} chunk(s) under {out} (method_validation_only)")
    if skipped:
        print(f"skipped {len(skipped)} recording(s):")
        for name, why in skipped:
            print(f"  - {name}: {why}")
    if sparse:
        print(f"{len(sparse)} recording(s) tagged pointer:absent_or_sparse "
              f"(<{MIN_POINTER_HZ} Hz) — exclude them before averaging any "
              f"mouse R^2, or the metric measures corpus idleness:")
        for name, hz in sparse:
            print(f"  - {name}: {hz} Hz")
    return {"chunks": total, "skipped": skipped, "sparse": sparse}


# --- R2 mirror fetch (#360): pull OWAMcap recordings from the external-
# datasets bucket. Credentials come from RCLONE_CONFIG_R2_* env vars only;
# the guard fails loudly because empty-string creds once made rclone look
# like a slow listing instead of a misconfiguration.
# The mirror was purged 2026-09-04 (owner ruling 2026-08-13, research/26 IDM
# stop-loss). The prefix is empty; re-mirror from HF before fetch() can work
# again -- the upstream dataset is CC BY-NC 4.0 and still byte-for-byte
# reproducible, unlike the VPT mirror that went with it.
R2_REMOTE = "r2:diogenes-external-datasets/d2e-480p/v2026.07/data"
R2_MIRROR_PURGED = "2026-09-04"
def _require_credentials() -> None:
    """Fail loudly and immediately on missing R2 credentials.

    Without this, rclone retries auth failures for minutes and looks like a slow
    listing rather than a misconfiguration -- which is exactly what happened
    once, because `gcloud secrets versions access` fails intermittently behind a
    proxy and silently exported empty strings.
    """
    import os
    missing = [k for k in ("RCLONE_CONFIG_R2_ACCESS_KEY_ID",
                           "RCLONE_CONFIG_R2_SECRET_ACCESS_KEY",
                           "RCLONE_CONFIG_R2_ENDPOINT")
               if not os.environ.get(k)]
    if missing:
        raise SystemExit(
            f"missing/empty R2 credentials: {missing}\n"
            "Export them from Secret Manager first (see "
            "model_research/research/19 §10). Note `gcloud secrets versions "
            "access` fails intermittently on this network -- check the values "
            "are non-empty, and retry the gcloud call rather than trusting it."
        )



def _require_mirror() -> None:
    """Fail loudly when the R2 mirror is empty.

    The prefix was purged on 2026-09-04, so a bare `fetch` used to list zero
    objects and exit 0 with an empty output directory -- indistinguishable from
    "nothing matched your filter". A `--games` fetch was worse: it raised
    "no such game(s) in the mirror", which reads like a typo in the game name.
    """
    # Depth-1 without --files-only: the mirror stores recordings as
    # <game>/<name>.mcap, so a files-only listing at the root is empty even
    # when the mirror is full.
    listing = _rclone(["lsf", "--max-depth", "1", R2_REMOTE])
    if listing.strip():
        return
    raise SystemExit(
        f"D2E R2 mirror is empty -- purged {R2_MIRROR_PURGED} to stop paying for "
        f"corpora with no consumer (owner ruling 2026-08-13; research/26 IDM "
        f"stop-loss).\n  prefix: {R2_REMOTE}\n"
        "Re-mirror from the upstream HF dataset before using `fetch` "
        "(CC BY-NC 4.0, byte-for-byte reproducible; procedure in "
        "model_research/research/28-idm-open-datasets-and-r2-ingestion.md STAGE 4)."
    )


def _rclone(args: list[str], *, timeout: int = 900) -> str:
    _require_credentials()
    out = subprocess.run(
        # bound the damage of a bad endpoint: fail in ~1 min, not ~10
        ["rclone", *args, "--contimeout", "20s", "--retries", "2",
         "--low-level-retries", "3"],
        capture_output=True, text=True, timeout=timeout)
    if out.returncode != 0:
        raise RuntimeError(f"rclone {' '.join(args)} failed: {out.stderr[-400:]}")
    return out.stdout



def session_of(name: str) -> str:
    """`rec_x_split_04` and `rec_x_split_12` are ONE session (see docstring #3)."""
    stem = name.rsplit(".", 1)[0]
    if "_split_" in stem:
        stem = stem.rsplit("_split_", 1)[0]
    return stem



def fetch(out: Path, games: list[str] | None, max_recordings: int | None,
          events_only: bool) -> None:
    _require_mirror()
    out.mkdir(parents=True, exist_ok=True)
    by_game: dict[str, list[str]] = {}
    if games:
        # List only the prefixes asked for. A recursive listing of the whole
        # mirror is ~920 objects and takes minutes over a slow link -- there is
        # no reason to pay for it to fetch two games.
        for game in games:
            listing = _rclone(["lsf", "--files-only", f"{R2_REMOTE}/{game}"])
            names = [n for n in listing.splitlines() if n.endswith(".mcap")]
            if names:
                by_game[game] = names
        unknown = sorted(set(games) - set(by_game))
        if unknown:
            raise SystemExit(f"no such game(s) in the mirror: {unknown}")
    else:
        listing = _rclone(["lsf", "-R", "--files-only", R2_REMOTE])
        for rel in listing.splitlines():
            if "/" not in rel or not rel.endswith(".mcap"):
                continue
            game, name = rel.split("/", 1)
            by_game.setdefault(game, []).append(name)

    for game, names in sorted(by_game.items()):
        picked = sorted(names)[:max_recordings] if max_recordings else sorted(names)
        for name in picked:
            dest = out / game
            dest.mkdir(parents=True, exist_ok=True)
            _rclone(["copy", f"{R2_REMOTE}/{game}/{name}", str(dest)])
            if not events_only:
                _rclone(["copy", f"{R2_REMOTE}/{game}/{name[:-5]}.mkv", str(dest)])
            print(f"fetched {game}/{name}{'' if events_only else ' (+mkv)'}")


# ---------------------------------------------------------------------------
# convert
# ---------------------------------------------------------------------------




def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch", help="pull recordings from the R2 mirror")
    f.add_argument("--out", type=Path, required=True)
    f.add_argument("--games", nargs="*", default=None,
                   help="directory names as in the mirror, e.g. Counter-Strike_2")
    f.add_argument("--max-recordings", type=int, default=None)
    f.add_argument("--events-only", action="store_true",
                   help="skip the .mkv (label-side work is MB not GB)")
    c = sub.add_parser("convert", help="OWAMcap pairs -> chunk contract")
    c.add_argument("--raw", required=True, type=Path,
                   help="dir of <name>.mcap + <name>.mkv (game per subdir)")
    c.add_argument("--out", required=True, type=Path)
    c.add_argument("--res", type=int, nargs=2, default=[224, 224])
    c.add_argument("--chunk-frames", type=int, default=120)
    c.add_argument("--codec", choices=["npz", "mp4"], default="mp4")
    c.add_argument("--target-fps", type=float, default=None,
                   help="resample onto a fixed wall-clock grid (sums deltas "
                        "per output interval); refuses to upsample. Default "
                        "keeps each recording's own measured rate")
    c.add_argument("--max-chunks", type=int, default=None,
                   help="per recording, for a quick probe")
    args = ap.parse_args()
    if args.cmd == "fetch":
        fetch(args.out, args.games, args.max_recordings, args.events_only)
        return
    rep = convert(args.raw, args.out, res=tuple(args.res),
                  chunk_frames=args.chunk_frames, codec=args.codec,
                  target_fps=args.target_fps, max_chunks=args.max_chunks)
    if rep["skipped"] and not rep["chunks"]:
        raise SystemExit("every recording was skipped — nothing to train on")


if __name__ == "__main__":
    main()
