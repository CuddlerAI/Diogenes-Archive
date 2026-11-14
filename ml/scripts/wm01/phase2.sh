#!/usr/bin/env bash
# WM 0.1 · Phase 2 动作模块训练（34 号文 §4 Phase 2 配方 + 35 号文 §3.3 消融臂）。
# LoRA 冻结（Phase 1 产物）、只训 ActionConditioner、lr 1e-4、bs1×accum4×4 卡 DP。
# （34 号文记 13.7M 是按 img_hidden_size=1536 估的；实际 DiT dim=3072 → 19.97M，
#   训练启动时 [ActionModule] 行会打印实测值。）
#
# 三臂由 PHASE2_ARM 环境变量选择（默认 native）：
#   native       A1 原生真值动作        action_dir=plane/action         confidence_modulate=false
#   pseudo       A2 IDM 伪标签均匀权重  action_dir=plane/action_pseudo  confidence_modulate=false
#   pseudo_conf  A3 伪标签 c_t 加权     action_dir=plane/action_pseudo  confidence_modulate=true
#
# 其他环境变量：
#   PHASE2_STEPS        max_iters，默认 2000（实测 ~2.5min/步；预算见 34 号文 §9 的 ±2× 修正）
#   PHASE2_SAVE_EVERY   log_iters=存档间隔，默认 50（save_every 必设，止损纪律）。
#                      注意：trainer 的步数计数器从 Phase 1 LoRA ckpt 继承（checkpoint_model_000500
#                      → 从 501 起步），首个存档落在 step 500+SAVE_EVERY。2026-08-03 实测
#                      ~2.2min/步、A100 spot 抢占间隔 2–5h：SAVE_EVERY=250 意味着首档前
#                      ~9h 无保护，抢占即全部重来（node2 一天内白跑 3 轮）。50 步≈110min
#                      一档。
#                      🟥 存档不是「只有几十 MB」：checkpoint 目录除 80MB 的 conditioner
#                      还落一份 ~14GB 的 error buffer 分片（error_recycling 的 t-bucket
#                      转储）。50 步节奏 × 旧的 max_checkpoints=10 稳态吃 ~157GB——
#                      2026-08-05 node2 因此把 387GB 盘写满，正在写的 ckpt 被截断，
#                      auto-resume 反复加载坏档、崩溃循环 10 次。故轮转上限设 3
#                      （见下 max_checkpoints）：resume 只用最新档、postarm 只认
#                      checkpoint_model_002000，留 3 档两者都安全。
#   PHASE2_MOUSE_SCALE  鼠标位移归一化分母，默认 200.0（2026-08-02 对 24fps 平面
#                      action 实测标定：逐帧 |mouse| p99=218 / p99.9=568 / max=1340，
#                      取 ≈p99 → 典型值 [-1,1]、甩枪尾部 ~6.7。原注：
#                       raw HID 位移和可到数百 counts/帧，除 100 拉回 O(1)。
#                       上真跑前建议对 plane/action 抽样量 p99 再定。）
#
# 依赖（顺序敏感）：
#   1. trainer/diffusion.py 已依次打过 patch_longlive_lora.py、patch_longlive_phase2.py；
#      utils/dataset.py 已打过 patch_longlive_phase2.py
#   2. Phase 1 LoRA 产物在 ~/runs/phase1/checkpoint_model_*/model.pt（脚本自动取最新）
#   3. ~/mlcode/ml 下有 diogenes_wm（补丁内 sys.path 注入，无需 PYTHONPATH）
set -euo pipefail
# a2 单机 NVLink：镜像 profile.d 为 a3u/gIB 预设的 NCCL_NET=gIB 在无 IB 机上直接炸掉 NET 插件初始化
unset NCCL_NET NCCL_TUNER_CONFIG_PATH NCCL_NET_GDR_LEVEL NCCL_IB_TC NCCL_IB_FIFO_TC NCCL_IB_QPS_PER_CONNECTION NCCL_IB_ADAPTIVE_ROUTING 2>/dev/null || true
unset LD_LIBRARY_PATH 2>/dev/null || true   # profile.d 的 gib lib64 会影子 torch 动态库
export PATH="$HOME/.local/bin:$PATH"
cd "$HOME/LongLive"
PLANE="$HOME/data/plane"
TRAIN="$HOME/data/train"

ARM="${PHASE2_ARM:-native}"
case "$ARM" in
  native)      ACTION_DIR="$PLANE/action";        CONF_MOD=False ;;
  pseudo)      ACTION_DIR="$PLANE/action_pseudo"; CONF_MOD=False ;;
  pseudo_conf) ACTION_DIR="$PLANE/action_pseudo"; CONF_MOD=True ;;
  *) echo "未知 PHASE2_ARM='$ARM'（可选 native|pseudo|pseudo_conf）" >&2; exit 2 ;;
esac
STEPS="${PHASE2_STEPS:-2000}"
SAVE_EVERY="${PHASE2_SAVE_EVERY:-50}"
MOUSE_SCALE="${PHASE2_MOUSE_SCALE:-200.0}"
OUT="$HOME/runs/phase2-$ARM"
mkdir -p "$OUT"

[ -d "$ACTION_DIR" ] || { echo "action_dir 不存在: $ACTION_DIR" >&2; exit 3; }

# Phase 1 LoRA：取 logdir 里最新的 checkpoint（冻结，不再训）
LORA_CKPT=$(ls -t "$HOME/runs/phase1"/checkpoint_model_*/model.pt 2>/dev/null | head -1)
[ -n "$LORA_CKPT" ] || { echo "找不到 Phase 1 LoRA checkpoint（~/runs/phase1/checkpoint_model_*/model.pt）" >&2; exit 3; }
echo "arm=$ARM action_dir=$ACTION_DIR conf_mod=$CONF_MOD lora_ckpt=$LORA_CKPT"
echo "steps=$STEPS save_every=$SAVE_EVERY mouse_scale=$MOUSE_SCALE"

echo "== 切 train split（plane 去掉 holdout 尾 2 会话，与 phase0/1 同口径）=="
rm -rf "$TRAIN" && mkdir -p "$TRAIN/video" "$TRAIN/caption"
HOLD=$(ls "$PLANE/video" | sort | tail -2)
for s in $(ls "$PLANE/video" | sort); do
  if ! grep -qx "$s" <<< "$HOLD"; then
    ln -s "$PLANE/video/$s" "$TRAIN/video/$s"
    ln -s "$PLANE/caption/$s" "$TRAIN/caption/$s"
  fi
done
echo "train sessions: $(ls "$TRAIN/video" | wc -l)（应为 17）"
# 注：action npz 不需要切分——训练侧按 segments_meta 里的会话名直查 $ACTION_DIR，
# holdout 会话根本不会出现在 train 采样里。

echo "== 生成训练配置 =="
# 非引号 heredoc：$LORA_CKPT/$ACTION_DIR/$MOUSE_SCALE/$CONF_MOD/$STEPS/$SAVE_EVERY/$ARM 由 shell 展开
.venv/bin/python - <<PY
import os
from omegaconf import OmegaConf
home = os.environ["HOME"]
c = OmegaConf.load("configs/train_i2v_ar.yaml")
c.infra.sequence_parallel_size = 1   # 动作模块补丁不支持 sp>1（token 折叠假设整段在本 rank）；4 卡改 DP
c.adapter = OmegaConf.create({"type": "lora", "rank": 64, "alpha": 64, "dropout": 0.0})
c.generator_ckpt = f"{home}/weights/LongLive-2.0-5B/model_bf16.pt"
c.lora_ckpt = "$LORA_CKPT"           # Phase 1 产物；phase2 补丁使 LoRA 不做 logdir auto-resume
c.action_module = OmegaConf.create({
    "enabled": True,
    "action_dir": "$ACTION_DIR",
    "blocks_fraction": 0.5,          # DiT 前一半 block（MG2 实时预算做法）
    "mouse_scale": $MOUSE_SCALE,
    "confidence_modulate": $CONF_MOD,
    "freeze_backbone": True,         # 冻结 generator 全部参数（含 LoRA）
})
c.training.gradient_accumulation_steps = 4   # 34 号文：bs1 × accum4 × 4 卡 = 有效 16
c.training.lr = 1.0e-4               # 新模块，比 LoRA 高一档
c.training.max_iters = $STEPS
c.training.log_iters = $SAVE_EVERY   # 存档节奏（action_conditioner-only ckpt）
c.training.ema_weight = 0.0          # 短跑关 EMA
c.training.max_checkpoints = 3   # 每档带 ~14GB error buffer：10 档会写满 387GB 盘（见文件头 SAVE_EVERY 注）
c.data.data_path = f"{home}/data/train"
c.data.eval_data_path = f"{home}/data/holdout"
c.data.image_or_video_shape = [1, 24, 48, 30, 52]   # 93 帧 → 24 潜帧, 480x832 → 30x52
c.evaluation.interval = 0            # 训练期不做生成评估（推理路径未接动作，触发即 raise）
OmegaConf.save(c, f"{home}/runs/phase2-$ARM/train_phase2.yaml")
print("config written")
PY

echo "== 训练（torchrun 4×A100，DP）=="
.venv/bin/torchrun --standalone --nnodes=1 --nproc_per_node=4 train.py \
  --config_path "$OUT/train_phase2.yaml" --logdir "$OUT" --disable-wandb
echo "== phase2-$ARM done =="
