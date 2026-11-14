"""IDM = frozen trunk + small NON-CAUSAL head (<50M trainable, hard-checked).

The head sees the whole window t-2..t+2 — a 3D-conv motion stem over raw
pixels (VPT-style first layer) plus bidirectional attention over per-frame
trunk features — and predicts the CENTER frame's action:
  keys   multi-hot over SCANNED_VKEYS      (BCE)
  bins   foveated pointer bins, x and y    (CE, 21 classes each)
  delta  raw pointer delta / 200           (regression, for the R^2 gate)
"""
from __future__ import annotations

import torch
import torch.nn as nn

from ..keys import N_BINS, N_KEYS
from .trunk import Trunk

TRAINABLE_BUDGET = 50_000_000  # Phase 0 plan: total trainable params < 50M


class MotionStem(nn.Module):
    """3D conv over grayscale + temporal-difference channels.

    Returns a 3x3 grid of spatial motion tokens — global pooling would drown
    small localized motion (a moving avatar) in dominant global motion (camera
    pan), and both are supervision targets.
    """

    GRID = 3
    SCALES = (1, 4)  # shared-weight pyramid: large per-frame motion (fast pan)
    # aliases past a 5px kernel at full res; the /4 branch brings it in range

    def __init__(self, window: int, d_model: int):
        super().__init__()
        self.window = window
        self.conv3d = nn.Conv3d(2, 32, kernel_size=(window, 5, 5),
                                stride=(1, 2, 2), padding=(0, 2, 2))
        self.net2d = nn.Sequential(
            nn.GELU(),
            nn.Conv2d(32, 64, 3, stride=2, padding=1), nn.GELU(),
            nn.Conv2d(64, 128, 3, stride=2, padding=1), nn.GELU(),
            nn.AdaptiveAvgPool2d(self.GRID),
        )
        self.proj = nn.Linear(128, d_model)
        self.scale_emb = nn.Parameter(torch.zeros(len(self.SCALES), 1, d_model))

    @property
    def n_tokens(self) -> int:
        return len(self.SCALES) * self.GRID ** 2

    def forward(self, frames: torch.Tensor) -> torch.Tensor:  # [B,T,3,h,w]
        assert frames.shape[1] == self.window
        gray = frames.mean(dim=2, keepdim=True)              # [B,T,1,h,w]
        diff = torch.diff(gray, dim=1, prepend=gray[:, :1])  # first-order motion
        x0 = torch.cat([gray, diff], dim=2).permute(0, 2, 1, 3, 4)  # [B,2,T,h,w]
        outs = []
        for si, s in enumerate(self.SCALES):
            x = x0 if s == 1 else torch.nn.functional.avg_pool3d(x0, (1, s, s))
            x = self.conv3d(x).squeeze(2)            # time collapsed -> [B,32,h',w']
            x = self.net2d(x)                        # [B,128,G,G]
            tokens = self.proj(x.flatten(2).transpose(1, 2))  # [B,G*G,D]
            outs.append(tokens + self.scale_emb[si])
        return torch.cat(outs, dim=1)                # [B,n_tokens,D]


class IDMHead(nn.Module):
    def __init__(self, trunk_dim: int, *, window: int = 5, d_model: int = 128,
                 n_layers: int = 2, n_heads: int = 4):
        super().__init__()
        self.window = window
        self.center = window // 2
        self.frame_proj = nn.Linear(trunk_dim, d_model)
        self.motion = MotionStem(window, d_model)
        self.pos = nn.Parameter(torch.zeros(1, window + self.motion.n_tokens, d_model))
        layer = nn.TransformerEncoderLayer(
            d_model, n_heads, dim_feedforward=4 * d_model,
            batch_first=True, norm_first=True, dropout=0.0)
        self.encoder = nn.TransformerEncoder(layer, n_layers,
                                             enable_nested_tensor=False)
        self.norm = nn.LayerNorm(d_model)
        self.head_keys = nn.Linear(d_model, N_KEYS)
        self.head_bin_x = nn.Linear(d_model, N_BINS)
        self.head_bin_y = nn.Linear(d_model, N_BINS)
        self.head_delta = nn.Linear(d_model, 2)

    def forward(self, feats: torch.Tensor, frames: torch.Tensor) -> dict[str, torch.Tensor]:
        tokens = self.frame_proj(feats)                       # [B,T,D]
        motion = self.motion(frames)                          # [B,G*G,D]
        x = torch.cat([tokens, motion], dim=1) + self.pos
        x = self.encoder(x)                                   # bidirectional == non-causal
        z = self.norm(x[:, self.center] + x[:, self.window:].mean(dim=1))
        return {
            "keys": self.head_keys(z),
            "bin_x": self.head_bin_x(z),
            "bin_y": self.head_bin_y(z),
            "delta": self.head_delta(z),
        }


class IDM(nn.Module):
    def __init__(self, trunk: Trunk, head: IDMHead):
        super().__init__()
        self.trunk = trunk
        self.head = head

    def forward(self, frames: torch.Tensor) -> dict[str, torch.Tensor]:
        feats = self.trunk(frames)
        return self.head(feats, frames)


def trainable_params(model: nn.Module) -> int:
    return sum(p.numel() for p in model.parameters() if p.requires_grad)


def build_model(cfg: dict) -> IDM:
    m = cfg.get("model", {})
    trunk = Trunk(m.get("trunk", "tiny"))
    head = IDMHead(
        trunk.dim,
        window=cfg.get("data", {}).get("window", 5),
        d_model=m.get("d_model", 128),
        n_layers=m.get("n_layers", 2),
        n_heads=m.get("n_heads", 4),
    )
    model = IDM(trunk, head)
    n = trainable_params(model)
    if n >= TRAINABLE_BUDGET:
        raise ValueError(
            f"trainable params {n:,} exceed the Phase 0 budget "
            f"{TRAINABLE_BUDGET:,} — the trunk must stay frozen and the head small")
    return model
