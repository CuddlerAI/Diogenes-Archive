"""BC 损失: 语义动作 BCE + camera 双轴 CE, burn-in 掩码 (research/07 §2.2)。"""
from __future__ import annotations

import torch
import torch.nn.functional as F


def bc_loss(
    action_logits: torch.Tensor,   # [B,T,A]
    camera_logits: torch.Tensor,   # [B,T,2,C]
    action_target: torch.Tensor,   # [B,T,A] float 0/1
    camera_target: torch.Tensor,   # [B,T,2] long ∈ [0,C)
    *,
    burn_in: int = 0,
    action_weight: float = 1.0,
    pos_weight: torch.Tensor | None = None,   # [A]
    label_smoothing: float = 0.05,
) -> dict[str, torch.Tensor]:
    if burn_in:
        action_logits = action_logits[:, burn_in:]
        camera_logits = camera_logits[:, burn_in:]
        action_target = action_target[:, burn_in:]
        camera_target = camera_target[:, burn_in:]

    bce = F.binary_cross_entropy_with_logits(
        action_logits, action_target, pos_weight=pos_weight)
    c = camera_logits.shape[-1]
    ce_x = F.cross_entropy(camera_logits[:, :, 0].reshape(-1, c),
                           camera_target[:, :, 0].reshape(-1),
                           label_smoothing=label_smoothing)
    ce_y = F.cross_entropy(camera_logits[:, :, 1].reshape(-1, c),
                           camera_target[:, :, 1].reshape(-1),
                           label_smoothing=label_smoothing)
    total = action_weight * bce + ce_x + ce_y
    return {"loss": total, "bce_actions": bce.detach(),
            "ce_yaw": ce_x.detach(), "ce_pitch": ce_y.detach()}


def estimate_pos_weight(labels: torch.Tensor, clamp_max: float = 50.0) -> torch.Tensor:
    """labels [N,A] 0/1 → BCE pos_weight = neg/pos (类不平衡校正)。"""
    pos = labels.sum(0).clamp(min=1.0)
    neg = labels.shape[0] - pos
    return (neg / pos).clamp(1.0, clamp_max)
