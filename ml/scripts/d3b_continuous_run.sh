#!/bin/bash
# research/30 D-3-b — does continuous decoding change the verdict?
#
#   systemd-run --user --unit=d3b --collect \
#       --working-directory=/home/oratis/ml --setenv=HOME=/home/oratis \
#       /home/oratis/ml/scripts/d3b_continuous_run.sh
#
# D-3 scored G-IDM on isolated 4-second episodes and found it had no measurable
# increment over `trivial.persist_seed`. The standing objection is that the
# protocol, not the model, produced that: upstream decodes ~2-minute segments,
# so a 4-second episode may simply never let it establish state. research/30 §7
# lists this as the change that would invalidate §4.
#
# So: the SAME chunks, decoded two ways.
#   chunk mode  one episode per chunk           (the D-3 protocol)
#   run mode    one episode per contiguous run  (~160 s, upstream-like)
# Paired on chunk id, so the difference is the protocol and nothing else.
#
# Only the 448 condition runs — D-3 measured the resolution effect at +0.009 /
# +0.039 and it changed no conclusion, so spending half the GPU on it again
# would buy a number we already have.
#
# ~4 h on an L4. Deliberately NOT `set -e`: a mid-run failure must still reach
# the trap, or a crash throws away hours of GPU and leaves the instance up.
#
#   D3_NO_STOP=1   keep the VM up (use while iterating)
#   D3_CHUNKS=N    contiguous chunks per split (default 40)
set -uo pipefail

ML=${ML:-/home/oratis/ml}
D3=${D3:-/home/oratis/d3}
RUN=${RUN:-d3b-continuous-$(date +%Y%m%d-%H%M%S)}
OUT=$D3/$RUN
GCS=gs://hakkolite-vl-scratch/idm/runs
CKPT=${CKPT:-$ML/runs/idm-d2e-zeroshot-20260801-042206/checkpoint.pt}
CFG=${CFG:-$ML/configs/idm_d2e_zeroshot.yaml}
ROOT=${ROOT:-$ML/data/d2e448}
N=${D3_CHUNKS:-40}
PY=/home/oratis/venv/bin/python
UV=/home/oratis/.local/bin/uv
PREDICT="$UV run --script $ML/diogenes_phase0/ext_idm/predict.py --d2e-repo /home/oratis/D2E"
ZONE=us-east1-b
PROJECT=hakkolite
INSTANCE=$(hostname -s)

mkdir -p "$OUT"
exec > >(tee -a "$OUT/run.log") 2>&1
echo "=== D-3-b continuous-decoding run $RUN (contiguous chunks/split=$N)"

finish() {
    code=$?
    echo "=== finishing (exit=$code); uploading $OUT -> $GCS/"
    gcloud storage cp -r "$OUT" "$GCS/" && echo "upload OK" || echo "UPLOAD FAILED"
    if [ "${D3_NO_STOP:-0}" = "1" ]; then
        echo "D3_NO_STOP=1 — leaving the VM up"
        exit $code
    fi
    echo "=== stopping $INSTANCE"
    gcloud compute instances stop "$INSTANCE" --zone=$ZONE --project=$PROJECT --quiet \
        || sudo poweroff
    exit $code
}
trap finish EXIT INT TERM

cd "$ML" || exit 1
{
    echo "run=$RUN"
    echo "checkpoint=$CKPT"
    echo "root=$ROOT"
    echo "contiguous_chunks_per_split=$N"
    echo "d2e_repo_commit=$(git -C /home/oratis/D2E rev-parse HEAD 2>/dev/null)"
    echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader)"
} > "$OUT/provenance.txt"

for SPLIT in val zero_shot; do
    echo "=== sampling contiguous $SPLIT"
    $PY -m diogenes_phase0.ext_idm.sample --config "$CFG" --root "$ROOT" \
        --split "$SPLIT" --n "$N" --contiguous --seed 0 \
        --out "$OUT/chunks_$SPLIT.txt" \
        --out-decode "$OUT/decode_$SPLIT.txt" || exit 1
done

# Inference first for BOTH modes and BOTH splits, then all scoring. A scoring
# bug after three hours of decoding costs a rerun of the scoring only.
for SPLIT in val zero_shot; do
    echo "=== [$SPLIT] predicting — chunk mode (one episode per chunk)"
    $PREDICT --chunks "$OUT/chunks_$SPLIT.txt" --out "$OUT/preds_${SPLIT}_chunk"

    # run mode decodes the DECODE list: every chunk in the run, including the
    # ones min_key_frac filtered out. They are valid observations, and leaving
    # them out would splice a cut into the footage at every dropped chunk.
    # Scoring still happens on the score list, so the two modes stay paired.
    echo "=== [$SPLIT] predicting — run mode (one episode per contiguous run)"
    $PREDICT --chunks "$OUT/decode_$SPLIT.txt" --out "$OUT/preds_${SPLIT}_run" --concat
done

for SPLIT in val zero_shot; do
    for MODE in chunk run; do
        echo "=== [$SPLIT/$MODE] scoring"
        $PY -m diogenes_phase0.ext_idm.score \
            --chunks "$OUT/chunks_$SPLIT.txt" \
            --pred-dir "$OUT/preds_${SPLIT}_${MODE}" \
            --config "$CFG" --checkpoint "$CKPT" \
            --label "${SPLIT}@${MODE}" --primary-shift-ms 100 \
            --episode-mode "$MODE" \
            --out "$OUT/report_${SPLIT}_${MODE}.json"
    done
done

echo "=== tables"
$PY -m diogenes_phase0.ext_idm.summarize \
    "$OUT"/report_val_run.json "$OUT"/report_val_chunk.json \
    "$OUT"/report_zero_shot_run.json "$OUT"/report_zero_shot_chunk.json \
    > "$OUT/tables.md"
cat "$OUT/tables.md"
echo "=== done"
