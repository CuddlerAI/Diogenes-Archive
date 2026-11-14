#!/usr/bin/env bash
# Runs the synced Track-A LAM probes (numpy, standalone) as the latent-action
# control line for Phase 0. See track_a_lam/README.md for what each gate means.
set -euo pipefail
cd "$(dirname "$0")/../.."
PY=${PY:-python3}
DIR=model_research/from_hakkolite/synced-model-knowledge/world-model-research/track_a_lam

for probe in leakage_probe.py effect_align.py transfer_probe.py minimal_lam_demo.py; do
  echo "=== $probe ==="
  (cd "$DIR" && $PY "$probe")
done
