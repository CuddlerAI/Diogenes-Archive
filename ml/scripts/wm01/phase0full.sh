#!/usr/bin/env bash
# WM 0.1 · Phase 0 全量零样本基线：holdout 逐 clip 首帧 I2V rollout（A0 臂的分母）。
# 冒烟版（runs/phase0）只出了 1 条：数据集把「会话」当条目（2 个），且同 caption 覆盖同名文件。
# 本版：holdout 展平成每 clip 一个条目 + save_with_index 命名 + inference_iter=-1 全量。
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
cd "$HOME/LongLive"
FLAT="$HOME/data/holdout_flat"
OUT="$HOME/runs/phase0full"
mkdir -p "$OUT"

echo "== 展平 holdout（每 clip 一个条目，附 manifest）=="
rm -rf "$FLAT" && mkdir -p "$FLAT/video" "$FLAT/caption"
for s in "$HOME/data/holdout/video"/*/; do
  sess=$(basename "$s")
  for v in "$s"*.mp4; do
    stem=$(basename "$v" .mp4)
    mkdir -p "$FLAT/video/$sess-$stem" "$FLAT/caption/$sess-$stem"
    ln -s "$v" "$FLAT/video/$sess-$stem/"
    ln -s "$HOME/data/holdout/caption/$sess/$stem.json" "$FLAT/caption/$sess-$stem/"
  done
done
ls "$FLAT/video" | sort > "$OUT/manifest.txt"
echo "holdout clips: $(wc -l < "$OUT/manifest.txt")"

echo "== 推理配置（在冒烟版 yaml 基础上改）=="
.venv/bin/python - <<'PY'
import os
from omegaconf import OmegaConf
home = os.environ["HOME"]
c = OmegaConf.load(f"{home}/runs/phase0/inference_i2v.yaml")
c.data.data_path = f"{home}/data/holdout_flat"
c.output_folder = f"{home}/runs/phase0full/rollouts"
c.save_with_index = True     # rank0-<idx>-<seed>_regular.mp4，idx 对齐 manifest 行号（0 起）
c.inference_iter = -1        # 全量
c.num_samples = 1
OmegaConf.save(c, f"{home}/runs/phase0full/inference_i2v.yaml")
PY

echo "== 推理（单卡全量，保完整性不用 DistributedSampler 的 drop_last）=="
.venv/bin/python inference.py --config_path "$OUT/inference_i2v.yaml" 2>&1 | tail -20

N=$(find "$OUT/rollouts" -name "*.mp4" | wc -l)
M=$(wc -l < "$OUT/manifest.txt")
echo "rollouts: $N / manifest: $M"
if [ "$N" -lt "$M" ]; then echo "PHASE0FULL_INCOMPLETE ($N < $M)"; exit 1; fi
echo "== phase0full done =="
