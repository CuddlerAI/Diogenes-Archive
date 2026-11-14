#!/usr/bin/env bash
# VL line end-to-end smoke: synth data -> bench -> stub teacher -> LoRA SFT
# (tiny random model) -> eval report. Mechanics only; needs the [vl] extra
# and network for the tiny HF test model on first run.
set -euo pipefail
cd "$(dirname "$0")/.."
PY=${PY:-python}

$PY -m diogenes_phase0.data.synthetic --out data/synth_smoke --seed 7
$PY -m diogenes_vl.bench --roots data/synth_smoke --out data/bench_synth \
    --per-game 24 --frame-stride 30 --skip-first-s 0
$PY -m diogenes_vl.teacher --bench data/bench_synth --backend stub \
    --out data/bench_synth/labels.jsonl
$PY -m diogenes_vl.sft --config configs/vl_smoke.yaml
ADAPTER=$(ls -td runs/vl-smoke-*/adapter | head -1)
$PY -m diogenes_vl.evaluate --config configs/vl_smoke.yaml \
    --adapter "$ADAPTER" --split val --limit 8
