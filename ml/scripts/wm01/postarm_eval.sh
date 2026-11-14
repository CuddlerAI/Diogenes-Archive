#!/usr/bin/env bash
# 臂后自动评测：等训练单元退出 → 最新 ckpt 做动作条件化 rollout（真值条件，全臂统一协议）
# → 重链名 → E1a 评分 → 产物上云。用法: ARM=native|pseudo|pseudo_conf UNIT=<训练单元名> bash postarm_eval.sh
set -euo pipefail
unset NCCL_NET NCCL_TUNER_CONFIG_PATH NCCL_NET_GDR_LEVEL LD_LIBRARY_PATH 2>/dev/null || true
export PATH="$HOME/.local/bin:$PATH"
ARM="${ARM:?}"; UNIT="${UNIT:?}"
export ARM UNIT
OUT="$HOME/runs/e1a-$ARM"

# 1. 完成信号 = ckpt_2000 存在（重启竞态免疫：抢占/续跑随便发生，等到终点为止）；
#    然后等训练单元彻底退出（最终存档后自然结束）
until [ -f "$HOME/runs/phase2-$ARM/checkpoint_model_002000/model.pt" ]; do sleep 180; done
while systemctl --user is-active --quiet "$UNIT" 2>/dev/null || systemctl --user is-active --quiet arm-auto 2>/dev/null; do sleep 60; done
sleep 30
CKPT=$(ls -t "$HOME/runs/phase2-$ARM"/checkpoint_model_*/model.pt 2>/dev/null | head -1)
[ -n "$CKPT" ] || { echo "NO_CKPT_ABORT"; exit 1; }
echo "eval ckpt: $CKPT"

# 2. 动作条件化 rollout（真值条件——三臂统一测「对同一动作流的跟随」；A3 训练期输入
#    是 conf 调制（均值 0.969），真值=1.0 在其分布上沿，偏移 ~3% 可忽略，协议记录在案）
mkdir -p "$OUT"
cd "$HOME/LongLive"
PYTHONPATH="$HOME/mlcode/ml" .venv/bin/python "$HOME/e1a_arm_rollout.py" \
  --action-ckpt "$CKPT" \
  --lora-ckpt "$HOME/runs/phase1/checkpoint_model_000500/model.pt" \
  --action-source native --allow-cross-arm --out "$OUT/rollouts" > "$OUT/rollout.log" 2>&1
N=$(find "$OUT/rollouts" -name "*.mp4" | wc -l)
echo "rollouts: $N"; [ "$N" -ge 120 ] || { echo "ROLLOUT_INCOMPLETE($N)"; exit 1; }

# 3. 重链成 flat 名（score_rollout 按 stem 匹配動作 npz）
PYTHONPATH="$HOME/mlcode/ml" .venv/bin/python - <<'PY'
import os
from pathlib import Path
home = Path.home(); arm = os.environ["ARM"]
man = [l.split("\t")[1] for l in sorted(
    (home/f"runs/e1a-{arm}/pairs.txt").read_text().splitlines(),
    key=lambda l: int(l.split("\t")[0]))]  # idx 有序的真实映射（对位错排修复）
ws = home/f"runs/e1a-{arm}/score_ws"; (ws/"rollouts").mkdir(parents=True, exist_ok=True)
src_actions = home/"runs/phase0full/score_ws/actions"   # 真值动作链已建
for i, flat in enumerate(man):
    mp4 = home/f"runs/e1a-{arm}/rollouts/rank0-{i}-0_regular.mp4"
    if mp4.exists():
        dst = ws/f"rollouts/{flat}.mp4"
        if not dst.exists(): dst.symlink_to(mp4)
print("linked", len(list((ws/"rollouts").iterdir())))
PY

# 4. E1a 评分（与 A0 基线同一入口/同一动作真值）
cd "$HOME/mlcode/ml"
PYTHONPATH="$HOME/mlcode/ml" "$HOME/LongLive/.venv/bin/python" -m diogenes_wm.wm01.score_rollout \
  --rollouts "$OUT/score_ws/rollouts" \
  --actions "$HOME/runs/phase0full/score_ws/actions" \
  --label "arm-$ARM" --out "$OUT/e1a_report.md" --json "$OUT/e1a_report.json" \
  > "$OUT/score.log" 2>&1

# 5. 上云
gsutil -q cp "$OUT/e1a_report.md" "$OUT/e1a_report.json" "gs://hakkolite-vl-scratch/wm01/arms/$ARM/"
gsutil -q cp "$CKPT" "gs://hakkolite-vl-scratch/wm01/arms/$ARM/model.pt"
echo "POSTARM_EVAL_DONE($ARM)"
