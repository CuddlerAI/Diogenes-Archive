"""Frozen per-frame encoders ("大的部分是免费的" — the trunk is never trained).

Options:
  tiny            deterministic random-init conv net, D=128 — offline CI smoke
  dinov2_vits14   torch.hub DINOv2 ViT-S/14, D=384 — real Phase 0 runs (needs
                  network on first load; input res must be a multiple of 14,
                  e.g. 224)
"""
from __future__ import annotations

import torch
import torch.nn as nn

_IMAGENET_MEAN = torch.tensor([0.485, 0.456, 0.406]).view(1, 3, 1, 1)
_IMAGENET_STD = torch.tensor([0.229, 0.224, 0.225]).view(1, 3, 1, 1)


class _TinyConv(nn.Module):
    dim = 128

    def __init__(self):
        super().__init__()
        gen = torch.Generator().manual_seed(0)
        self.net = nn.Sequential(
            nn.Conv2d(3, 32, 5, stride=2, padding=2), nn.GELU(),
            nn.Conv2d(32, 64, 3, stride=2, padding=1), nn.GELU(),
            nn.Conv2d(64, 128, 3, stride=2, padding=1), nn.GELU(),
            nn.AdaptiveAvgPool2d(1),
        )
        for p in self.net.parameters():  # deterministic frozen features
            with torch.no_grad():
                p.copy_(torch.randn(p.shape, generator=gen) * 0.1)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.net(x).flatten(1)


class Trunk(nn.Module):
    """Wraps a frozen encoder; forward maps [B,T,3,h,w] -> [B,T,D]."""

    def __init__(self, name: str):
        super().__init__()
        self.name = name
        if name == "tiny":
            self.encoder: nn.Module = _TinyConv()
            self.dim = _TinyConv.dim
        elif name.startswith("dinov2"):
            self.encoder = torch.hub.load("facebookresearch/dinov2", name)
            self.dim = self.encoder.embed_dim
        else:
            raise ValueError(f"unknown trunk '{name}'")
        self.encoder.eval()
        for p in self.encoder.parameters():
            p.requires_grad_(False)
        self.register_buffer("mean", _IMAGENET_MEAN, persistent=False)
        self.register_buffer("std", _IMAGENET_STD, persistent=False)

    def train(self, mode: bool = True):  # trunk stays in eval mode forever
        super().train(mode)
        self.encoder.eval()
        return self

    @torch.no_grad()
    def forward(self, frames: torch.Tensor) -> torch.Tensor:
        b, t = frames.shape[:2]
        x = frames.flatten(0, 1)
        x = (x - self.mean) / self.std
        feats = self.encoder(x)
        return feats.view(b, t, self.dim)
