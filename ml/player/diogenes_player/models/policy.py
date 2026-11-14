"""Player Policy: frozen encoder → adapter → GRU → 双动作头 (keys BCE ⊕ mouse μ-law CE)。

设计: research/04 §4.1。encoder 可插拔:
  tiny_cnn   ~0.1M  测试/CPU 冒烟 (无下载依赖)
  effnet_b0  5.3M   torchvision 预训练, 冻结 (W2 起的默认真 encoder)
  dinov3_s   21M    TODO(CS1): HF 权重 + Gate-0 预检探针后接入

step() 与 forward() 共享权重, 数值一致性由测试锁定 (导出链 W3 依赖此性质)。
"""
from __future__ import annotations

from dataclasses import dataclass, field

import torch
from torch import nn

from .. import contract


@dataclass
class PolicyConfig:
    encoder: str = "tiny_cnn"
    d_model: int = 256
    gru_hidden: int = 512
    gru_layers: int = 1
    n_actions: int = 26          # BC 主目标：语义动作数（COD=26；随 vocab 变）
    n_keys: int = contract.N_KEYS  # frozen PhysicalInputVocab width; legacy default only
    n_bins: int = contract.N_BINS
    aux_keys: bool = False       # 是否启用 keys 辅助头
    freeze_encoder: bool = True
    imagenet_norm: bool = field(default=False)  # build_encoder 会按需覆写


class TinyCNN(nn.Module):
    out_dim = 128

    def __init__(self) -> None:
        super().__init__()
        chans = [3, 16, 32, 64]
        layers: list[nn.Module] = []
        for cin, cout in zip(chans[:-1], chans[1:]):
            layers += [nn.Conv2d(cin, cout, 3, stride=2, padding=1),
                       nn.GroupNorm(4, cout), nn.SiLU()]
        self.net = nn.Sequential(*layers)
        # 全局池化: ONNX 导出为 GlobalAveragePool, 任意输入尺寸合法 (W3 导出链要求)
        self.pool = nn.AdaptiveAvgPool2d(1)
        self.fc = nn.Linear(64, self.out_dim)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.fc(self.pool(self.net(x)).flatten(1))


def build_encoder(name: str, freeze: bool) -> tuple[nn.Module, int, bool]:
    """→ (module, out_dim, needs_imagenet_norm)"""
    if name == "tiny_cnn":
        enc = TinyCNN()
        if freeze:  # tiny_cnn 一般可训练; 冻结仅用于对照
            _freeze(enc)
        return enc, enc.out_dim, False
    if name == "effnet_b0":
        from torchvision.models import EfficientNet_B0_Weights, efficientnet_b0

        m = efficientnet_b0(weights=EfficientNet_B0_Weights.DEFAULT)
        enc = nn.Sequential(m.features, m.avgpool, nn.Flatten(1))
        if freeze:
            _freeze(enc)
        return enc, 1280, True
    if name == "dinov3_s":
        raise NotImplementedError(
            "dinov3_s 接入排在 CS1（需 HF 权重 + Gate-0 UI-OOD 预检探针, 见 research/04 §4.1）")
    raise ValueError(f"unknown encoder {name!r}")


def _freeze(m: nn.Module) -> None:
    m.eval()
    for p in m.parameters():
        p.requires_grad_(False)


_IMAGENET_MEAN = torch.tensor([0.485, 0.456, 0.406]).view(1, 3, 1, 1)
_IMAGENET_STD = torch.tensor([0.229, 0.224, 0.225]).view(1, 3, 1, 1)


class PlayerPolicy(nn.Module):
    def __init__(self, cfg: PolicyConfig) -> None:
        super().__init__()
        self.cfg = cfg
        self.encoder, enc_dim, self.imagenet_norm = build_encoder(
            cfg.encoder, cfg.freeze_encoder)
        self.proj = nn.Linear(enc_dim, cfg.d_model)
        self.gru = nn.GRU(cfg.d_model, cfg.gru_hidden, cfg.gru_layers,
                          batch_first=True)
        self.action_head = nn.Linear(cfg.gru_hidden, cfg.n_actions)   # 主
        self.camera_head = nn.Linear(cfg.gru_hidden, 2 * cfg.n_bins)  # yaw/pitch
        self.keys_head = nn.Linear(cfg.gru_hidden, cfg.n_keys) if cfg.aux_keys else None

    def _encode(self, frames: torch.Tensor) -> torch.Tensor:
        """frames float32 [B,T,3,H,W] (0..1) → [B,T,d_model]"""
        b, t = frames.shape[:2]
        x = frames.flatten(0, 1)
        if self.imagenet_norm:
            x = (x - _IMAGENET_MEAN.to(x)) / _IMAGENET_STD.to(x)
        feat = self.encoder(x)
        return self.proj(feat).view(b, t, -1)

    def forward(self, frames: torch.Tensor, h0: torch.Tensor | None = None):
        """→ action_logits [B,T,A], camera_logits [B,T,2,n_bins], h_n"""
        z, h_n = self.gru(self._encode(frames), h0)
        actions = self.action_head(z)
        camera = self.camera_head(z).view(*z.shape[:2], 2, self.cfg.n_bins)
        return actions, camera, h_n

    def aux_keys_logits(self, frames: torch.Tensor, h0=None):
        """辅助 keys 头（训练消融用；aux_keys=True 时）。"""
        if self.keys_head is None:
            raise RuntimeError("aux_keys 未启用")
        z, _ = self.gru(self._encode(frames), h0)
        return self.keys_head(z)

    @torch.no_grad()
    def step(self, frame: torch.Tensor, h: torch.Tensor):
        """单帧推理 (部署路径)。frame [B,3,H,W] → action_logits [B,A], camera_logits [B,2,n_bins], h'"""
        actions, camera, h_n = self.forward(frame.unsqueeze(1), h)
        return actions[:, 0], camera[:, 0], h_n

    def init_hidden(self, batch: int, device=None) -> torch.Tensor:
        return torch.zeros(self.cfg.gru_layers, batch, self.cfg.gru_hidden,
                           device=device)

    def num_params(self) -> dict:
        total = sum(p.numel() for p in self.parameters())
        trainable = sum(p.numel() for p in self.parameters() if p.requires_grad)
        return {"total": total, "trainable": trainable}
