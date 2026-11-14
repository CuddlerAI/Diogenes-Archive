"""训练平面上的随机时间窗采样器（R0：语义动作目标 + run 边界约束）。

返回样本:
    frames      uint8   [T, H, W, 3]
    actions     float32 [T, N_ACTIONS]    语义动作 multi-hot（BC 主目标；需 vocab）
    physical_inputs float32 [T, N_INPUTS] frozen VK + mouse-button multi-hot
    keys        float32 [T, N_INPUTS]     compatibility alias for aux head
    camera_bins int64   [T, 2]            softmax 类别 (bin + MAX_BIN, ∈[0,20])
    raw_delta   float32 [T, 2]            未截断真实 dx/dy（#137 pointerRaw；旧平面回退 clamp）；评估解码 R² 用
    meta        dict

治理: 默认拒绝 is_trainable=false；管线验证需显式 allow_untrainable=True（权重即弃）。
run 边界: 采样窗口不得跨 run_frame_range（失焦/暂停切断，research/07 §5）；data_demo
无 run range 时视整个 session 为单一 range。
"""
from __future__ import annotations

import json
import warnings
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

from . import contract
from .action_registry import ActionVocab
from .physical_vocab import PhysicalInputContractError, PhysicalInputVocab
from .video import decode_window


@dataclass
class _ChunkRef:
    video: str          # 相对 plane 根（权威路径）
    plane_rel: str      # session 目录相对路径（slug 化，actions.parquet / run_ranges 键）
    game: str
    session: str
    chunk: int
    row_offset: int
    n_frames: int
    gframe_start: int
    fps: float
    valid_starts: np.ndarray = field(default_factory=lambda: np.empty(0, dtype=np.int64))


def _valid_starts(gframe_start: int, n_frames: int, t: int,
                  ranges: list[dict]) -> np.ndarray:
    """返回窗口起点集合，使 [g0+s, g0+s+t) 完整落在单个 run range 内。"""
    if n_frames < t:
        return np.empty(0, dtype=np.int64)
    if not ranges:  # 无 run 信息 → 整段合法
        return np.arange(0, n_frames - t + 1, dtype=np.int64)
    out = []
    for s in range(n_frames - t + 1):
        w0, w1 = gframe_start + s, gframe_start + s + t  # [w0, w1) gframe 空间
        if any(r["frame_start"] <= w0 and w1 <= r["frame_end"] for r in ranges):
            out.append(s)
    return np.asarray(out, dtype=np.int64)


def _angle_pair(acts: dict, lo: int, hi: int) -> np.ndarray:
    """→ [T,2] float32 的 (yaw, pitch) 弧度；未标定列缺失或为 null 时填 NaN。"""
    n = hi - lo
    out = np.full((n, 2), np.nan, dtype=np.float32)
    for j, key in enumerate(("yaw_rad", "pitch_rad")):
        col = acts.get(key)
        if col is None:
            continue
        seg = col[lo:hi]
        out[:, j] = np.array([np.nan if v is None else float(v) for v in seg],
                             dtype=np.float32)
    return out


class WindowDataset:
    def __init__(self, plane_root: str | Path, *, t: int = 96,
                 vocab: ActionVocab | None = None,
                 physical_vocab: PhysicalInputVocab | None = None,
                 allow_untrainable: bool = False, backend: str = "auto",
                 seed: int = 0) -> None:
        self.root = Path(plane_root)
        self.t = t
        self.vocab = vocab
        self.backend = backend
        self.rng = np.random.default_rng(seed)

        vocab_path = self.root / "physical_input_vocab.json"
        if not vocab_path.exists():
            raise PhysicalInputContractError(
                f"{vocab_path} is missing; refusing an unversioned physical label plane")
        plane_physical_vocab = PhysicalInputVocab.read(vocab_path)
        if (physical_vocab is not None
                and physical_vocab.content_hash != plane_physical_vocab.content_hash):
            raise PhysicalInputContractError(
                "checkpoint/request physical vocab does not match training plane: "
                f"{physical_vocab.content_hash} != {plane_physical_vocab.content_hash}")
        self.physical_vocab = plane_physical_vocab

        index = pq.read_table(self.root / "index.parquet").to_pylist()
        bad_hashes = sorted({str(r.get("physical_vocab_hash")) for r in index
                             if r.get("physical_vocab_hash")
                             != self.physical_vocab.content_hash})
        if bad_hashes:
            raise PhysicalInputContractError(
                "index.parquet physical_vocab_hash mismatch: "
                f"expected {self.physical_vocab.content_hash}, found {bad_hashes}")
        dropped = [r for r in index if not r["is_trainable"]]
        if dropped and not allow_untrainable:
            index = [r for r in index if r["is_trainable"]]
        elif dropped:
            warnings.warn(
                f"WindowDataset: 使用 {len(dropped)} 个 is_trainable=false chunk"
                "（仅限管线验证，产出的权重不得发布）", stacklevel=2)
        if not index:
            raise ValueError(
                "无可用 chunk：数据集中没有 is_trainable=true 的数据；"
                "管线验证请显式传 allow_untrainable=True")

        rr_path = self.root / "run_ranges.json"
        run_ranges = (
            json.loads(rr_path.read_text(encoding="utf-8"))
            if rr_path.exists() else {}
        )

        self.chunks: list[_ChunkRef] = []
        for r in index:
            if r["n_frames"] < t:
                continue
            plane_rel = str(Path(r["actions_parquet"]).parent)  # slug 化 session 目录
            ranges = run_ranges.get(plane_rel, [])
            vs = _valid_starts(int(r["gframe_start"]), int(r["n_frames"]), t, ranges)
            if vs.size == 0:  # 整块被 run 边界切碎到放不下一个窗口
                continue
            self.chunks.append(_ChunkRef(
                video=r["video"], plane_rel=plane_rel,
                game=r["game"], session=r["session"],
                chunk=r["chunk"], row_offset=r["row_offset"],
                n_frames=r["n_frames"], gframe_start=int(r["gframe_start"]),
                fps=r["fps"], valid_starts=vs))
        if not self.chunks:
            raise ValueError(f"没有可容纳 T={t} 窗口（且不跨 run 边界）的 chunk")

        self._actions: dict[str, dict] = {}
        for prel in {c.plane_rel for c in self.chunks}:
            table = pq.read_table(self.root / prel / "actions.parquet")
            cols = set(table.column_names)
            # raw_dx/raw_dy = 未截断真实 Δ（#137）；旧平面无此列时回退 clamp 后的 dx/dy
            raw_dx = (table["raw_dx"] if "raw_dx" in cols else table["dx"]).to_numpy()
            raw_dy = (table["raw_dy"] if "raw_dy" in cols else table["dy"]).to_numpy()
            self._actions[prel] = {
                "physical_inputs_mask": table["physical_inputs_mask"].to_numpy(),
                "actions": table["actions"].to_pylist(),
                "bin_x": table["bin_x"].to_numpy(),
                "bin_y": table["bin_y"].to_numpy(),
                "raw_dx": raw_dx,
                "raw_dy": raw_dy,
                "yaw_rad": (table["look_yaw_delta_rad"].to_numpy(zero_copy_only=False)
                            if "look_yaw_delta_rad" in cols else None),
                "pitch_rad": (table["look_pitch_delta_rad"].to_numpy(zero_copy_only=False)
                              if "look_pitch_delta_rad" in cols else None),
                "view_valid": (table["view_scale_valid"].to_numpy(zero_copy_only=False)
                               if "view_scale_valid" in cols else None),
                "src": table["src"].to_numpy(zero_copy_only=False),
            }

    def __len__(self) -> int:
        return len(self.chunks)

    def sample(self) -> dict:
        c = self.chunks[self.rng.integers(len(self.chunks))]
        start = int(c.valid_starts[self.rng.integers(c.valid_starts.size)])
        return self.get(c, start)

    def get(self, c: _ChunkRef, start: int) -> dict:
        frames = decode_window(str(self.root / c.video), start, self.t,
                               fps=c.fps, backend=self.backend)
        acts = self._actions[c.plane_rel]
        lo = c.row_offset + start
        hi = lo + self.t

        physical_inputs = np.zeros(
            (self.t, self.physical_vocab.n_inputs), dtype=np.float32)
        for i, m in enumerate(acts["physical_inputs_mask"][lo:hi]):
            if m:
                physical_inputs[i] = self.physical_vocab.mask_to_multihot(int(m))

        n_act = self.vocab.n_actions if self.vocab else 0
        actions = np.zeros((self.t, n_act), dtype=np.float32)
        if self.vocab:
            for i, present in enumerate(acts["actions"][lo:hi]):
                if present:
                    actions[i] = self.vocab.to_multihot(present)

        camera = np.stack([
            acts["bin_x"][lo:hi].astype(np.int64) + contract.MAX_BIN,
            acts["bin_y"][lo:hi].astype(np.int64) + contract.MAX_BIN,
        ], axis=1)
        return {
            "frames": frames,
            "actions": actions,
            "physical_inputs": physical_inputs,
            "keys": physical_inputs,
            "camera_bins": camera,
            "raw_delta": np.stack([acts["raw_dx"][lo:hi], acts["raw_dy"][lo:hi]],
                                  axis=1).astype(np.float32),
            # 弧度制视角增量（docs/45）。未标定的平面这里全是 NaN，且 view_valid 全 False——
            # 调用方据此决定用角度还是回退到 μ-law 计数箱，不要静默混用。
            "look_delta_rad": _angle_pair(acts, lo, hi),
            "view_scale_valid": (acts["view_valid"][lo:hi].astype(bool)
                                 if acts["view_valid"] is not None
                                 else np.zeros(self.t, dtype=bool)),
            "src": acts["src"][lo:hi],
            "meta": {"game": c.game, "session": c.session,
                     "chunk": c.chunk, "start": start,
                     "physical_vocab_hash": self.physical_vocab.content_hash},
        }
