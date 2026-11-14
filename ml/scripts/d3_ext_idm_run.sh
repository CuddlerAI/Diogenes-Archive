#!/bin/bash
# research/26 D-3 — Generalist-IDM-1B against our head on the D2E eval set.
#
#   systemd-run --user --unit=d3-main --collect \
#       --working-directory=/home/oratis/ml --setenv=HOME=/home/oratis \
#       /home/oratis/ml/scripts/d3_ext_idm_run.sh
#
# ~4 h on an L4 (the model decodes at roughly 20x realtime, and the eval set is
# 160 four-second chunks). Everything lands under $OUT, is copied to
# gs://hakkolite-vl-scratch/idm/runs/ and then the VM stops itself.
#
# Deliberately NOT `set -e`: a mid-run failure must still reach the trap, or a
# crash at chunk 130 of 160 throws away three hours of GPU and leaves an
# instance running. `set -u` and `pipefail` stay on.
#
#   D3_NO_STOP=1   keep the VM up (use while iterating)
#   D3_CHUNKS=N    chunks per split per condition (default 40)
set -uo pipefail

ML=${ML:-/home/oratis/ml}
D3=${D3:-/home/oratis/d3}
RUN=${RUN:-d3-gidm1b-$(date +%Y%m%d-%H%M%S)}
OUT=$D3/$RUN
GCS=gs://hakkolite-vl-scratch/idm/runs
CKPT=${CKPT:-$ML/runs/idm-d2e-zeroshot-20260801-042206/checkpoint.pt}
CFG=${CFG:-$ML/configs/idm_d2e_zeroshot.yaml}
N=${D3_CHUNKS:-40}
PY=/home/oratis/venv/bin/python
UV=/home/oratis/.local/bin/uv
PREDICT="$UV run --script $ML/diogenes_phase0/ext_idm/predict.py --d2e-repo /home/oratis/D2E"
ZONE=us-east1-b
PROJECT=hakkolite
INSTANCE=$(hostname -s)

mkdir -p "$OUT"
exec > >(tee -a "$OUT/run.log") 2>&1
echo "=== D-3 external IDM run $RUN  (chunks/split/condition=$N)"

finish() {
    code=$?
    echo "=== finishing (exit=$code); uploading $OUT -> $GCS/"
    gcloud storage cp -r "$OUT" "$GCS/" && echo "upload OK" || echo "UPLOAD FAILED"
    if [ "${D3_NO_STOP:-0}" = "1" ]; then
        echo "D3_NO_STOP=1 — leaving the VM up"
        exit $code
    fi
    echo "=== stopping $INSTANCE"
    # API stop first; fall back to an in-guest poweroff, which also lands the
    # instance in TERMINATED, so a missing compute.instances.stop permission
    # cannot leave a GPU VM billing overnight.
    gcloud compute instances stop "$INSTANCE" --zone=$ZONE --project=$PROJECT \
        --quiet || sudo poweroff
}
trap finish EXIT INT TERM

cd "$ML" || exit 1

# --- 1. the eval set: sampled on d2e30 (the split of record), restricted to
# chunks that also exist at 448, so both resolution conditions score the same
# chunk ids rather than two overlapping samples.
for split in val zero_shot; do
    $PY -m diogenes_phase0.ext_idm.sample --config "$CFG" --root data/d2e30 \
        --split $split --n "$N" --seed 0 --require-exists data/d2e448 \
        --out "$OUT/chunks_${split}_224.txt" || exit 1
    sed 's#data/d2e30/#data/d2e448/#' "$OUT/chunks_${split}_224.txt" \
        > "$OUT/chunks_${split}_448.txt"
done

# --- 2. inference. 448 (native 480p source) first: it is the primary
# condition, so a run that dies halfway still answers the main question.
for cond in 448 224; do
    for split in val zero_shot; do
        echo "=== predict $split @$cond"
        $PREDICT --chunks "$OUT/chunks_${split}_${cond}.txt" \
                 --out "$OUT/preds_${split}_${cond}" || echo "PREDICT FAILED $split@$cond"
    done
done

# --- 3. scoring. Every system on one window set; score.py refuses to write a
# report if its accumulator disagrees with evaluate.run_eval.
for cond in 448 224; do
    for split in val zero_shot; do
        echo "=== score $split @$cond"
        $PY -m diogenes_phase0.ext_idm.score \
            --chunks "$OUT/chunks_${split}_${cond}.txt" \
            --pred-dir "$OUT/preds_${split}_${cond}" \
            --config "$CFG" --checkpoint "$CKPT" \
            --label "${split}@${cond}" \
            --shifts-ms "${D3_SHIFTS:-0,50,100,150,200}" \
            --primary-shift-ms "${D3_PRIMARY_SHIFT:-100}" \
            --out "$OUT/report_${split}_${cond}.json" \
            || echo "SCORE FAILED $split@$cond"
    done
done

# provenance: which checkpoint, which corpus, which upstream commit
{
    echo "run=$RUN"; echo "checkpoint=$CKPT"; echo "config=$CFG"
    echo "chunks_per_split=$N"
    echo "d2e_repo_commit=$(git -C /home/oratis/D2E rev-parse HEAD 2>/dev/null)"
    echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader)"
} > "$OUT/provenance.txt"

echo "=== done"
