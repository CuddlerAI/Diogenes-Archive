"""Action-driven synthetic dataset generator (contract format, $0 data).

Renders a tiny world where every scanned action has a distinct, learnable
visual consequence — so a working IDM must beat chance by a wide margin:

  mouse delta      pans the background texture (pan_t = delta_t * scale)
  W/A/S/D          move the avatar square (wraps at viewport edges)
  mouseL (vk=1)    frame-wide flash while held
  mouseR (vk=2)    dark vignette while held
  digits 1..6      recolor the avatar while held (rare -> per-class F1 signal)

An exogenous distractor square does a random walk the actions don't control,
so the head cannot solve the task by tracking "the moving thing".

Output layout mirrors data_demo: {game}/{session}/{cNNNN}/{p0.npz,p0.jsonl,meta.json}.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from ..keys import DATA_DEMO_LEGACY_KEY_SCHEMA, foveated_bin

PAN_SCALE = 0.15          # texture px per unit of mouse delta
AVATAR_SIZE = 14
AVATAR_SPEED = 5          # must stay >> distractor speed or keys are unlearnable
DIGIT_COLORS = np.array(
    [[255, 64, 64], [64, 255, 64], [64, 64, 255],
     [255, 255, 64], [255, 64, 255], [64, 255, 255]], dtype=np.float32
)
KEY_SCHEMA = DATA_DEMO_LEGACY_KEY_SCHEMA


def _texture(rng: np.random.Generator, size: int) -> np.ndarray:
    low = rng.random((size // 8, size // 8, 3)).astype(np.float32)
    tex = np.kron(low, np.ones((8, 8, 1), dtype=np.float32))
    tex = (tex + np.roll(tex, 1, 0) + np.roll(tex, 1, 1)) / 3.0
    return (40 + 140 * tex).astype(np.float32)  # keep headroom for the flash


def _script_actions(rng: np.random.Generator, n_frames: int) -> list[dict]:
    """Piecewise-constant key holds + smoothed mouse impulses."""
    frames: list[dict] = []
    t = 0
    while t < n_frames:
        seg = int(rng.integers(8, 30))
        # never sample contradictory pairs (W+S, A+D): their visual consequence
        # equals "no key" and the label becomes pure noise
        move = [vk for vk in (int(rng.choice([0, 65, 68])),
                              int(rng.choice([0, 87, 83]))) if vk]
        fire = rng.random() < 0.25
        aim = rng.random() < 0.15
        digit = int(rng.integers(49, 55)) if rng.random() < 0.12 else None
        digit_len = int(rng.integers(2, 6))
        mouse_active = rng.random() < 0.6
        vx = float(rng.normal(0, 60)) if mouse_active else 0.0
        vy = float(rng.normal(0, 40)) if mouse_active else 0.0
        for i in range(min(seg, n_frames - t)):
            vx = 0.85 * vx + (float(rng.normal(0, 12)) if mouse_active else 0.0)
            vy = 0.85 * vy + (float(rng.normal(0, 9)) if mouse_active else 0.0)
            dx = float(np.clip(round(vx), -200, 200))
            dy = float(np.clip(round(vy), -200, 200))
            keys = list(move)
            if fire:
                keys.append(1)
            if aim:
                keys.append(2)
            if digit is not None and i < digit_len:
                keys.append(digit)
            frames.append({"keys": sorted(keys), "dx": dx, "dy": dy})
        t += seg
    return frames[:n_frames]


def _render_session(rng: np.random.Generator, game_seed: int, n_frames: int,
                    res: int) -> tuple[np.ndarray, list[dict]]:
    tex = _texture(np.random.default_rng(game_seed), res * 4)
    actions = _script_actions(rng, n_frames)
    cam = np.array([0.0, 0.0])
    avatar = np.array([res / 2.0, res / 2.0])
    distractor = np.array([res / 4.0, res / 4.0])
    dvel = np.array([1.5, -1.0])

    ys, xs = np.mgrid[0:res, 0:res]
    border = res // 8
    vignette = ((xs < border) | (xs >= res - border)
                | (ys < border) | (ys >= res - border))

    frames = np.empty((n_frames, res, res, 3), dtype=np.uint8)
    for t, a in enumerate(actions):
        keys = set(a["keys"])
        cam += np.array([a["dx"], a["dy"]]) * PAN_SCALE
        vel = np.array([
            (68 in keys) * AVATAR_SPEED - (65 in keys) * AVATAR_SPEED,
            (83 in keys) * AVATAR_SPEED - (87 in keys) * AVATAR_SPEED,
        ], dtype=float)
        avatar = (avatar + vel) % res
        dvel += rng.normal(0, 0.25, 2)
        dvel = np.clip(dvel, -1.5, 1.5)  # << AVATAR_SPEED: keeps the exogenous
        # distractor's motion separable from action-driven avatar motion
        distractor = (distractor + dvel) % res

        rows = (np.arange(res) + int(round(cam[1]))) % tex.shape[0]
        cols = (np.arange(res) + int(round(cam[0]))) % tex.shape[1]
        img = tex[np.ix_(rows, cols)].copy()

        digit = next((k for k in keys if 49 <= k <= 54), None)
        color = DIGIT_COLORS[digit - 49] if digit else np.array([245.0, 245.0, 245.0])
        for pos, c in ((distractor, np.array([120.0, 120.0, 120.0])), (avatar, color)):
            x0, y0 = int(pos[0]), int(pos[1])
            xr = np.arange(x0, x0 + AVATAR_SIZE) % res
            yr = np.arange(y0, y0 + AVATAR_SIZE) % res
            img[np.ix_(yr, xr)] = c

        if 2 in keys:
            img[vignette] *= 0.45
        if 1 in keys:
            img = img * 0.55 + 255.0 * 0.45
        frames[t] = np.clip(img, 0, 255).astype(np.uint8)
    return frames, actions


def _chunk_meta(game: str, session: str, chunk_idx: int, n_frames: int,
                res: int, fps: int) -> dict:
    return {
        "sample_key": f"{session}_c{chunk_idx:04d}",
        "session": session,
        "chunk": chunk_idx,
        "game": game,
        "category": "synthetic",
        "platform": "synthetic",
        "video": {"file": "p0.npz", "codec": "npz", "width": res, "height": res,
                  "fps": fps, "frames": n_frames, "audio": False},
        "annotation_source": "synthetic_ground_truth",
        "key_schema": KEY_SCHEMA,
        "rights": "synthetic",
        "first_party": True,
        "consent_id": "synthetic",
        "is_trainable": True,
        "is_cutscene": False,
    }


def generate_dataset(out_root: str | Path, *, games: list[str],
                     sessions_per_game: int = 2, chunks_per_session: int = 6,
                     chunk_frames: int = 120, res: int = 96, fps: int = 30,
                     seed: int = 7) -> Path:
    out_root = Path(out_root)
    out_root.mkdir(parents=True, exist_ok=True)
    index_lines = []
    for gi, game in enumerate(games):
        for si in range(sessions_per_game):
            session = f"{game}_s{si:04d}"
            rng = np.random.default_rng(seed * 10_000 + gi * 100 + si)
            n_frames = chunks_per_session * chunk_frames
            frames, actions = _render_session(rng, game_seed=seed + gi, n_frames=n_frames, res=res)
            sess_dir = out_root / game / session
            for ci in range(chunks_per_session):
                cdir = sess_dir / f"c{ci:04d}"
                cdir.mkdir(parents=True, exist_ok=True)
                lo, hi = ci * chunk_frames, (ci + 1) * chunk_frames
                np.savez_compressed(cdir / "p0.npz", frames=frames[lo:hi])
                with open(cdir / "p0.jsonl", "w") as f:
                    for t in range(lo, hi):
                        a = actions[t]
                        f.write(json.dumps({
                            "frame": t - lo, "gframe": t,
                            "video_t_ms": round(t * 1000 / fps),
                            "keys": a["keys"],
                            "pointerDx": a["dx"], "pointerDy": a["dy"],
                            "pointerBins": [foveated_bin(a["dx"]), foveated_bin(a["dy"])],
                            "src": "synthetic",
                        }) + "\n")
                (cdir / "meta.json").write_text(json.dumps(
                    _chunk_meta(game, session, ci, chunk_frames, res, fps), indent=2))
            (sess_dir / "metadata.json").write_text(json.dumps({
                "workflow_id": session, "game": game, "category": "synthetic",
                "platform": "synthetic", "duration_ms": round(n_frames * 1000 / fps),
                "capture": {"fps": fps, "width": res, "height": res,
                            "frame_count": n_frames, "chunks": chunks_per_session,
                            "chunk_frames": chunk_frames},
                "rights": "synthetic", "first_party": True,
                "consent_id": "synthetic", "is_trainable": True,
            }, indent=2))
            index_lines.append(json.dumps({"workflow_id": session, "game": game,
                                           "frames": n_frames}))
    (out_root / "metadata.jsonl").write_text("\n".join(index_lines) + "\n")
    return out_root


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", required=True)
    p.add_argument("--games", nargs="+", default=["synth_arena", "synth_canyon"])
    p.add_argument("--sessions-per-game", type=int, default=2)
    p.add_argument("--chunks", type=int, default=6)
    p.add_argument("--chunk-frames", type=int, default=120)
    p.add_argument("--res", type=int, default=96)
    p.add_argument("--seed", type=int, default=7)
    args = p.parse_args()
    root = generate_dataset(args.out, games=args.games,
                            sessions_per_game=args.sessions_per_game,
                            chunks_per_session=args.chunks,
                            chunk_frames=args.chunk_frames,
                            res=args.res, seed=args.seed)
    print(f"synthetic dataset written to {root}")


if __name__ == "__main__":
    main()
