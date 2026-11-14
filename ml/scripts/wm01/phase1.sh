#!/usr/bin/env bash
# WM 0.1 · Phase 1 域适配 LoRA（34 号文配方：rank64/alpha64、lr 1e-5、bs1×accum4×4 卡、
# 4,000 步、每 500 步存档、动作模块不在图里、EMA 关）。
# 依赖：trainer/diffusion.py 的 LoRA 移植（distillation.py 同款模式，原件在 diffusion.py.orig）。
set -euo pipefail
# a2 单机 NVLink：镜像 profile.d 为 a3u/gIB 预设的 NCCL_NET=gIB 在无 IB 机上直接炸掉 NET 插件初始化
unset NCCL_NET NCCL_TUNER_CONFIG_PATH NCCL_NET_GDR_LEVEL NCCL_IB_TC NCCL_IB_FIFO_TC NCCL_IB_QPS_PER_CONNECTION NCCL_IB_ADAPTIVE_ROUTING 2>/dev/null || true
unset LD_LIBRARY_PATH 2>/dev/null || true   # profile.d 的 gib lib64 会影子 torch 动态库
export PATH="$HOME/.local/bin:$PATH"
cd "$HOME/LongLive"
PLANE="$HOME/data/plane"
TRAIN="$HOME/data/train"
OUT="$HOME/runs/phase1"
mkdir -p "$OUT"

echo "== 切 train split（plane 去掉 holdout 尾 2 会话，与 phase0 同口径）=="
rm -rf "$TRAIN" && mkdir -p "$TRAIN/video" "$TRAIN/caption"
HOLD=$(ls "$PLANE/video" | sort | tail -2)
for s in $(ls "$PLANE/video" | sort); do
  if ! grep -qx "$s" <<< "$HOLD"; then
    ln -s "$PLANE/video/$s" "$TRAIN/video/$s"
    ln -s "$PLANE/caption/$s" "$TRAIN/caption/$s"
  fi
done
echo "train sessions: $(ls "$TRAIN/video" | wc -l)（应为 17）"

echo "== 生成训练配置 =="
.venv/bin/python - <<'PY'
import os
from omegaconf import OmegaConf
home = os.environ["HOME"]
c = OmegaConf.load("configs/train_i2v_ar.yaml")
c.infra.sequence_parallel_size = 1   # 24 潜帧 % (sp*nfpb) 校验：sp=4 只配上游 96 潜帧；4 卡改 DP，正合 34 号文有效 batch 16
c.adapter = OmegaConf.create({"type": "lora", "rank": 64, "alpha": 64, "dropout": 0.0})
c.generator_ckpt = f"{home}/weights/LongLive-2.0-5B/model_bf16.pt"
c.training.gradient_accumulation_steps = 4   # 34 号文：bs1 × accum4 × 4 卡
c.training.max_iters = 4000
c.training.log_iters = 500                   # 存档节奏（LoRA-only ckpt）
c.training.ema_weight = 0.0                  # LoRA 短跑关 EMA
c.training.max_checkpoints = 10
c.data.data_path = f"{home}/data/train"
c.data.eval_data_path = f"{home}/data/holdout"
c.data.image_or_video_shape = [1, 24, 48, 30, 52]   # 93 帧 → 24 潜帧, 480x832 → 30x52
c.evaluation.interval = 0                    # 训练期不做生成评估（E 系评测独立跑）
OmegaConf.save(c, f"{home}/runs/phase1/train_phase1.yaml")
print("config written")
PY

echo "== 训练（torchrun 4×A100，sp=4）=="
.venv/bin/torchrun --standalone --nnodes=1 --nproc_per_node=4 train.py \
  --config_path "$OUT/train_phase1.yaml" --logdir "$OUT" --disable-wandb
echo "== phase1 done =="
