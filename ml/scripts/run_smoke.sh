#!/usr/bin/env bash
# End-to-end smoke: synthetic dataset -> train tiny IDM -> eval -> gate check.
# Exits non-zero if a smoke gate fails. PY=... to pick the interpreter.
set -euo pipefail
cd "$(dirname "$0")/.."
PY=${PY:-python}

$PY -m diogenes_phase0.data.synthetic --out data/synth_smoke --seed 7 \
    --sessions-per-game 3 --chunks 8
$PY -m diogenes_phase0.train --config configs/smoke.yaml --check-gates
