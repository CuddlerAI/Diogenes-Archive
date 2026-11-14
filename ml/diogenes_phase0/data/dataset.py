"""Discovery + windowed torch Dataset over contract-format chunk dirs.

A sample is a window of `window` consecutive frames (default 5 = t-2..t+2)
from a single chunk, with the CENTER frame's action as the target — the
non-causal IDM setup: the head may look at past AND future frames.
"""
from __future__ import annotations

import hashlib
import json
from collections import OrderedDict
from pathlib import Path

import numpy as np
import torch
from torch.utils.data import Dataset

from ..contract import (ActionFrame, ChunkMeta, ChunkRecord,
                        gate_fps, gate_phase0_contract, load_actions,
                        resolve_phase0_contract_mode)
from ..keys import MOUSE_CLAMP, bin_to_class, keys_to_multihot


def scan_dataset(root: str | Path) -> list[ChunkRecord]:
    """Find every chunk dir (has meta.json + p0.jsonl + frames) under root."""
    root = Path(root)
    records: list[ChunkRecord] = []
    for meta_path in sorted(root.rglob("meta.json")):
        chunk_dir = meta_path.parent
        if not (chunk_dir / "p0.jsonl").exists():
            continue
        if not ((chunk_dir / "p0.npz").exists() or (chunk_dir / "p0.mp4").exists()):
            continue
        meta = ChunkMeta.from_json(json.loads(meta_path.read_text()))
        n_frames = sum(1 for line in open(chunk_dir / "p0.jsonl") if line.strip())
        records.append(ChunkRecord(chunk_dir=chunk_dir, meta=meta, n_frames=n_frames))
    return records


def _load_frames(record: ChunkRecord) -> np.ndarray:
    path = record.frames_path
    if path.suffix == ".npz":
        return np.load(path)["frames"]
    try:
        import cv2
    except ImportError as e:  # pragma: no cover
        raise RuntimeError(
            f"{path} is a video but opencv is not installed; "
            "install the [data] extra to decode mp4 chunks"
        ) from e
    cap = cv2.VideoCapture(str(path))
    frames = []
    ok, frame = cap.read()
    while ok:
        frames.append(cv2.cvtColor(frame, cv2.COLOR_BGR2RGB))
        ok, frame = cap.read()
    cap.release()
    return np.stack(frames)


def _session_of(record: ChunkRecord) -> str:
    return record.chunk_dir.parent.name


def split_records(
    records: list[ChunkRecord],
    *,
    holdout_games: list[str],
    val_fraction: float = 0.2,
) -> dict[str, list[ChunkRecord]]:
    """Game-level zero-shot holdout + session-hash val split within train games."""
    holdout_lower = {g.lower() for g in holdout_games}
    zero_shot = [r for r in records if r.meta.game.lower() in holdout_lower]
    rest = [r for r in records if r.meta.game.lower() not in holdout_lower]
    train, val = [], []
    for r in rest:
        h = int(hashlib.sha1(_session_of(r).encode()).hexdigest(), 16) % 1000
        (val if h < val_fraction * 1000 else train).append(r)
    return {"train": train, "val": val, "zero_shot": zero_shot}


class WindowDataset(Dataset):
    """(window frames, center action) samples over a list of chunks."""

    def __init__(
        self,
        records: list[ChunkRecord],
        *,
        window: int = 5,
        stride: int = 1,
        input_res: tuple[int, int] = (96, 96),
        delta_scale: float = 50.0,
        delta_source: str = "pointer",
        delta_clamp: float | None = None,
        contract_mode: str | None = None,
        allow_method_validation_only: bool | None = None,
        expected_fps: float | None = None,
        cache_chunks: int = 8,
    ):
        assert window % 2 == 1, "window must be odd (center-frame prediction)"
        mode = resolve_phase0_contract_mode(
            contract_mode,
            allow_method_validation_only=allow_method_validation_only,
        )
        gate_phase0_contract([r.meta for r in records], contract_mode=mode)
        gate_fps([r.meta for r in records], expected_fps=expected_fps)
        self.records = records
        self.contract_mode = mode
        self.window = window
        self.input_res = tuple(input_res)
        # raw deltas are clamped to +-MOUSE_CLAMP then divided by delta_scale;
        # /MOUSE_CLAMP makes typical targets ~0.1 and the regression head
        # collapses to predicting 0, so scale by a typical-magnitude constant
        # instead (R^2 is invariant to the choice as long as it's consistent)
        self.delta_scale = float(delta_scale)
        # Which pointer representation the regression target comes from. The
        # default reproduces every run to date bit for bit; changing it changes
        # what mouse_r2 means, so it is a config decision and never a default.
        #   pointer  clamped pointerDx/Dy (saturates on flicks — see ActionFrame)
        #   raw      untruncated pointerRawDx/Dy, unclamped unless delta_clamp
        #   angle    calibrated radians; refuses chunks without view_scale_valid
        if delta_source not in ("pointer", "raw", "angle"):
            raise ValueError(f"unknown delta_source {delta_source!r}")
        self.delta_source = delta_source
        self.delta_clamp = (MOUSE_CLAMP if delta_source == "pointer" else None) \
            if delta_clamp is None else float(delta_clamp)
        self._cache: OrderedDict[int, tuple[np.ndarray, list[ActionFrame]]] = OrderedDict()
        self._cache_chunks = cache_chunks
        self.index: list[tuple[int, int]] = []  # (record_idx, center_t)
        half = window // 2
        for ri, rec in enumerate(records):
            for t in range(half, rec.n_frames - half, stride):
                self.index.append((ri, t))

    def __len__(self) -> int:
        return len(self.index)

    def _chunk(self, ri: int) -> tuple[np.ndarray, list[ActionFrame]]:
        if ri in self._cache:
            self._cache.move_to_end(ri)
            return self._cache[ri]
        rec = self.records[ri]
        frames = _load_frames(rec)
        actions = load_actions(rec.actions_path)
        n = min(len(frames), len(actions))
        item = (frames[:n], actions[:n])
        self._cache[ri] = item
        while len(self._cache) > self._cache_chunks:
            self._cache.popitem(last=False)
        return item

    def _delta_target(self, a: ActionFrame) -> tuple[float, float]:
        if self.delta_source == "angle":
            if not a.view_scale_valid or a.look_yaw_delta_rad is None \
                    or a.look_pitch_delta_rad is None:
                # Falling back to counts here would mix radians and counts in
                # one target column and the R^2 would still print a number.
                raise ValueError(
                    f"delta_source='angle' needs view-scale calibration, but "
                    f"frame {a.frame} has view_scale_valid="
                    f"{a.view_scale_valid} yaw={a.look_yaw_delta_rad}"
                )
            dx, dy = a.look_yaw_delta_rad, a.look_pitch_delta_rad
        elif self.delta_source == "raw":
            dx, dy = a.raw_dx, a.raw_dy
        else:
            dx, dy = a.pointer_dx, a.pointer_dy
        if self.delta_clamp is not None:
            c = self.delta_clamp
            dx, dy = float(np.clip(dx, -c, c)), float(np.clip(dy, -c, c))
        return float(dx), float(dy)

    def __getitem__(self, i: int) -> dict:
        ri, t = self.index[i]
        frames, actions = self._chunk(ri)
        half = self.window // 2
        clip = frames[t - half : t + half + 1]  # [T,H,W,3] uint8
        a = actions[t]

        x = torch.from_numpy(np.ascontiguousarray(clip)).permute(0, 3, 1, 2).float() / 255.0
        if x.shape[-2:] != self.input_res:
            x = torch.nn.functional.interpolate(
                x, size=self.input_res, mode="bilinear", align_corners=False
            )

        dx, dy = self._delta_target(a)
        return {
            "frames": x,  # [T,3,h,w] float 0..1
            "keys": torch.tensor(keys_to_multihot(a.keys), dtype=torch.float32),
            "bin_x": torch.tensor(bin_to_class(a.pointer_bins[0]), dtype=torch.long),
            "bin_y": torch.tensor(bin_to_class(a.pointer_bins[1]), dtype=torch.long),
            "delta": torch.tensor([dx / self.delta_scale, dy / self.delta_scale],
                                  dtype=torch.float32),
            "game": self.records[ri].meta.game,
        }


def action_density(record: ChunkRecord) -> dict:
    """How much learnable action signal a chunk actually carries.

    A 14-session probe of gaming-500 found whole games with ~0% keyboard
    activity — gamepad titles (Tekken, RPCS3) and mouse-only strategy
    (Stronghold). Their frames are perfectly good pixels paired with an
    all-zero action label, so an IDM trained on them learns "predict
    nothing" and the key metric silently measures class imbalance instead
    of recovery.
    """
    actions = load_actions(record.actions_path)
    if not actions:
        return {"n": 0, "key_frac": 0.0, "mouse_frac": 0.0}
    n_key = sum(1 for a in actions if a.keys)
    n_mouse = sum(1 for a in actions if a.pointer_dx or a.pointer_dy)
    return {"n": len(actions), "key_frac": n_key / len(actions),
            "mouse_frac": n_mouse / len(actions)}


def filter_by_action_density(records: list[ChunkRecord], *,
                             min_key_frac: float = 0.10,
                             min_frames: int = 20) -> tuple[list[ChunkRecord], dict]:
    """Keep chunks with enough keyboard signal to be learnable.

    Returns (kept, stats). Games are reported in the stats so a dropped
    title is visible rather than silently missing from the run.
    """
    kept, dropped = [], []
    for r in records:
        d = action_density(r)
        if d["n"] >= min_frames and d["key_frac"] >= min_key_frac:
            kept.append(r)
        else:
            dropped.append((r.meta.game, d["key_frac"]))
    by_game: dict[str, list[float]] = {}
    for g, kf in dropped:
        by_game.setdefault(g, []).append(kf)
    stats = {
        "kept": len(kept), "dropped": len(dropped),
        "dropped_games": {g: round(sum(v) / len(v), 4)
                          for g, v in sorted(by_game.items())},
        "min_key_frac": min_key_frac,
    }
    return kept, stats
