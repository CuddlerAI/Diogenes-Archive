"""WindowDataset 的 torch 封装: IterableDataset + collate (research/04 §3.2)。

每个 dataloader worker 持有独立 WindowDataset 实例（解码器句柄不 fork 安全），
seed = base_seed + worker_id 保证互不重复。
"""
from __future__ import annotations

import numpy as np
import torch
from torch.utils.data import IterableDataset, get_worker_info

from .dataset import WindowDataset


def sample_to_tensors(s: dict) -> dict:
    frames = torch.from_numpy(np.ascontiguousarray(s["frames"]))  # [T,H,W,3] u8
    return {
        "frames": frames.permute(0, 3, 1, 2).float().div_(255.0),  # [T,3,H,W]
        "actions": torch.from_numpy(s["actions"]),
        "physical_inputs": torch.from_numpy(s["physical_inputs"]),
        "keys": torch.from_numpy(s["keys"]),
        "camera_bins": torch.from_numpy(s["camera_bins"]),
        "raw_delta": torch.from_numpy(s["raw_delta"]),
    }


class BCIterable(IterableDataset):
    def __init__(self, plane_root: str, *, t: int = 96,
                 samples_per_epoch: int = 1024, allow_untrainable: bool = False,
                 backend: str = "auto", seed: int = 0, vocab=None,
                 physical_vocab=None) -> None:
        self.plane_root = plane_root
        self.t = t
        self.n = samples_per_epoch
        self.allow_untrainable = allow_untrainable
        self.backend = backend
        self.seed = seed
        self.vocab = vocab
        self.physical_vocab = physical_vocab

    def __iter__(self):
        info = get_worker_info()
        wid, nw = (info.id, info.num_workers) if info else (0, 1)
        ds = WindowDataset(self.plane_root, t=self.t, vocab=self.vocab,
                           physical_vocab=self.physical_vocab,
                           allow_untrainable=self.allow_untrainable,
                           backend=self.backend, seed=self.seed + wid)
        for _ in range(self.n // nw):
            yield sample_to_tensors(ds.sample())


def make_loader(plane_root: str, *, t: int, batch_size: int, workers: int,
                samples_per_epoch: int, allow_untrainable: bool = False,
                 backend: str = "auto", seed: int = 0,
                 vocab=None, physical_vocab=None) -> torch.utils.data.DataLoader:
    return torch.utils.data.DataLoader(
        BCIterable(plane_root, t=t, samples_per_epoch=samples_per_epoch,
                   allow_untrainable=allow_untrainable, backend=backend, seed=seed,
                   vocab=vocab, physical_vocab=physical_vocab),
        batch_size=batch_size, num_workers=workers,
        pin_memory=torch.cuda.is_available(), drop_last=True,
        persistent_workers=workers > 0)
