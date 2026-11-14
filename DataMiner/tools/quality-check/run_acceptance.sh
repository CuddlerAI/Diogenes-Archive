#!/usr/bin/env bash
# Loop-有位移 一条 session 的端到端验收（docs/128）。
#
#   ./run_acceptance.sh <session-id> [out-dir]
#   ./run_acceptance.sh --local <已下载的 session 目录> [out-dir]     # SID=<id> 可覆盖标题里的 session 名
#
# 顺序是有讲究的：**先跑内容闸（第 3 步），再花时间转换**。转换要几分钟，而内容项
# （HUD 没关、录进菜单、静止过半、片中改 FOV）一条就废掉整条，早失败早省时间。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
BUCKET="diogenes-model-training"

usage() { sed -n '2,9p' "$0" | sed 's/^# \?//'; exit 2; }

LOCAL=""
if [[ "${1:-}" == "--local" ]]; then
  LOCAL="${2:-}"; shift 2
  # 目录名常常不是 session id（比如就叫 raw/），所以允许 SID= 覆盖
  SID="${SID:-$(basename "${LOCAL%/}")}"
elif [[ $# -ge 1 ]]; then SID="$1"; shift
else usage; fi
OUT="${1:-$HOME/Documents/diogenes-capture-runs/session-${SID:0:8}-validation-$(date +%Y%m%d)}"

mkdir -p "$OUT"/{raw,reports,frames,package}
echo "==> session=$SID  out=$OUT"

# --- 0 · python 环境 --------------------------------------------------------
VENV="$OUT/qcenv"
if [[ ! -x "$VENV/bin/python" ]]; then
  python3 -m venv "$VENV"
  "$VENV/bin/pip" -q install --upgrade pip
  "$VENV/bin/pip" -q install opencv-python-headless numpy
fi
PY="$VENV/bin/python"

# --- 1 · 取数 ---------------------------------------------------------------
if [[ -n "$LOCAL" ]]; then
  RAW="$LOCAL"
else
  # R2 凭据走 GCP Secret Manager（项目 hakkolite），不落配置文件。
  # gcloud 在本机代理下会偶发 RemoteDisconnected，所以直读 + 重试，别先 list。
  sm() { for _ in 1 2 3 4 5; do
           v=$(gcloud secrets versions access latest --secret="$1" --project=hakkolite 2>/dev/null) \
             && { printf '%s' "$v"; return 0; }; sleep 2; done; return 1; }
  export RCLONE_CONFIG_R2_TYPE=s3 RCLONE_CONFIG_R2_PROVIDER=Cloudflare RCLONE_CONFIG_R2_REGION=auto
  RCLONE_CONFIG_R2_ACCESS_KEY_ID="$(sm r2-training-access-key-id)"
  RCLONE_CONFIG_R2_SECRET_ACCESS_KEY="$(sm r2-training-secret-access-key)"
  RCLONE_CONFIG_R2_ENDPOINT="https://$(sm r2-training-account-id).r2.cloudflarestorage.com"
  export RCLONE_CONFIG_R2_ACCESS_KEY_ID RCLONE_CONFIG_R2_SECRET_ACCESS_KEY RCLONE_CONFIG_R2_ENDPOINT

  RAW="$OUT/raw"
  FOUND=""
  for LANE in attested-trainable non-trainable; do
    SRC="r2:$BUCKET/raw/v1/$LANE/dataminer-api-production/sessions/$SID"
    if rclone lsf --max-depth 1 "$SRC" >/dev/null 2>&1 \
       && [[ -n "$(rclone lsf --max-depth 1 "$SRC" 2>/dev/null)" ]]; then
      echo "==> lane=$LANE"; echo "$LANE" > "$OUT/lane.txt"
      rclone copy "$SRC" "$RAW" --transfers 8 --multi-thread-streams 4 --stats 20s --stats-one-line
      FOUND=1; break
    fi
  done
  [[ -n "$FOUND" ]] || { echo "!! 两条 lane 都找不到 session $SID"; exit 1; }
fi
echo "==> raw: $(find "$RAW" -type f | wc -l | tr -d ' ') files, $(du -sh "$RAW" | cut -f1)"

# --- 2 · SOP v2 内容闸（A–F，直接吃原始件，不需要转换） ---------------------
echo "==> [1/4] SOP v2 视频侧自检"
set +e
"$PY" "$HERE/sopv2_video_check.py" "$RAW" --out "$OUT/_sopv2" > "$OUT/reports/sopv2_report.md" 2> "$OUT/_sopv2.log"
SOPV2_RC=$?
set -e
cat "$OUT/reports/sopv2_report.md"

FOCAL=$("$PY" - "$OUT/_sopv2/sopv2_result.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
f=(d["fov_measured"].get("after_change") or {}).get("f_1280")
print(f if f else 836.4)
PY
)
SPLIT=$("$PY" - "$OUT/_sopv2/sopv2_result.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
# 读字段，不从判定文案里抠帧号——文案会变（比如内参可读时那一项换了写法）。
print(0 if d.get("fov_constant", False) else int(d.get("zoom_split_frame") or 0))
PY
)
echo "==> 标定焦距 f_1280=$FOCAL，FOV 跳变帧=$SPLIT"

# --- 3 · 轨迹重建 + 闭环 ----------------------------------------------------
echo "==> [2/4] RGB-D 里程计（无位姿索引时的唯一轨迹来源）"
FB=$("$PY" - "$OUT/_sopv2/sopv2_result.json" <<'PY'
import json,sys
d=json.load(open(sys.argv[1]))
f=(d["fov_measured"].get("before_change") or {}).get("f_1280")
print(f if f else 0)
PY
)
[[ "$FB" == "0" ]] && FB="$FOCAL"
"$PY" "$HERE/rgbd_odometry.py" "$RAW" "$OUT/odom.json" \
  --video "$OUT/_sopv2/session_full.mp4" \
  --switch-frame "$SPLIT" --f-before "$FB" --f-after "$FOCAL" | tee "$OUT/reports/odometry.json"

# --- 4 · 核验用五件套 -------------------------------------------------------
echo "==> [3/4] 组装核验用五件套（非交付件：位姿是里程计重建）"
PKG="$OUT/package/$(date +%Y_%m_%d)_${SID:0:8}_60s"
mkdir -p "$PKG"
ffmpeg -y -v error -i "$OUT/_sopv2/session_full.mp4" \
  -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
  -frames:v 1800 -vf "scale=1920:1080:flags=lanczos" -r 30 -vsync cfr \
  -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p -c:a aac -b:a 128k -shortest \
  "$PKG/video.mp4"
"$PY" "$HERE/build_check_package.py" "$PKG" "$OUT/odom.json" "$RAW" --focal "$FOCAL"

# --- 5 · 四个脚本 + 截图 ----------------------------------------------------
echo "==> [4/4] 客户脚本 + 闭环 + 手持感 + depth↔RGB 对齐"
set +e
"$PY" "$HERE/quality_check_depth_dataset.py" "$PKG" > "$OUT/reports/customer_report.md"; CUST_RC=$?
"$PY" "$HERE/loop_closure_check.py"          "$PKG" > "$OUT/reports/loop_report.md"
"$PY" "$HERE/handheld_feel_check.py"         "$PKG" > "$OUT/reports/handheld_report.md"
"$PY" "$HERE/depth_rgb_alignment_check.py"   "$PKG" > "$OUT/reports/alignment_report.md"; ALIGN_RC=$?
set -e
cat "$OUT/reports/customer_report.md" "$OUT/reports/loop_report.md" \
  "$OUT/reports/handheld_report.md" "$OUT/reports/alignment_report.md"

"$PY" "$HERE/render_report.py" "$OUT/reports" "$OUT/reports/acceptance.html" \
  --session "$SID" --lane "$(cat "$OUT/lane.txt" 2>/dev/null || echo local)"
echo
echo "==> 报告：$OUT/reports/acceptance.html （截图交业务方）"
echo "==> SOP v2 退出码 =$SOPV2_RC，客户脚本退出码 =$CUST_RC，对齐闸退出码 =$ALIGN_RC"
echo "==> 提醒：客户脚本只查结构；内容判定以 SOP v2 + 闭环 + 对齐闸 + 抽帧目检为准。"
exit $(( SOPV2_RC != 0 || CUST_RC != 0 || ALIGN_RC != 0 ))
