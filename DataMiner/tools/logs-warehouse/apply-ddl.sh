#!/usr/bin/env bash
# Apply the BigQuery DDL + scheduled MERGE for one environment (docs/23 §7).
#
#   ./apply-ddl.sh staging          # tables only
#   ./apply-ddl.sh staging schedule # + create the hourly MERGE scheduled query
#   ./apply-ddl.sh staging verify   # acceptance query from docs/23 §7.1
#
# 幂等：CREATE OR REPLACE / IF NOT EXISTS；schedule 先查重再建。
set -euo pipefail

PROJECT="${PROJECT:-hakkolite}"
REGION="${REGION:-us-central1}"
EXPORTER_SA="${EXPORTER_SA:-dataminer-log-exporter@${PROJECT}.iam.gserviceaccount.com}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ENV="${1:?usage: $0 <staging|production> [schedule|verify]}"
case "${ENV}" in staging|production) ;; *) echo "env must be staging|production" >&2; exit 2 ;; esac

log() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

render() { sed "s/\${ENV}/${ENV}/g" "$1"; }

tables() {
  # Hive 分区外部表要求前缀下至少存在一个对象；空 NDJSON（0 行）seed 满足它
  # 而不引入假数据。幂等：存在即跳过。
  local seed="gs://dataminer-logs/${ENV}/client_events/dt=1970-01-01/hour=00/000000000000-000000000000.ndjson.gz"
  if ! gcloud storage objects describe "${seed}" >/dev/null 2>&1; then
    log "Seed empty object ${seed}"
    printf '' | gzip | gcloud storage cp - "${seed}"
  fi
  log "External table client_events_raw (${ENV})"
  render "${HERE}/ddl/client_events_raw.sql" | bq --project_id="${PROJECT}" query --use_legacy_sql=false --format=none
  log "Native table client_events (${ENV})"
  render "${HERE}/ddl/client_events.sql" | bq --project_id="${PROJECT}" query --use_legacy_sql=false --format=none
}

schedule() {
  local name="dataminer-logs-merge-${ENV}"
  log "Hourly MERGE scheduled query ${name} (runs as ${EXPORTER_SA})"
  if bq --project_id="${PROJECT}" ls --transfer_config=true --transfer_location="${REGION}" --format=json 2>/dev/null \
      | python3 -c "
import json, sys
raw = sys.stdin.read().strip()
configs = json.loads(raw) if raw else []
sys.exit(0 if any(c.get('displayName') == '${name}' for c in configs) else 1)"; then
    echo "scheduled query ${name} already exists"
    return
  fi
  local query
  query="$(render "${HERE}/ddl/load_merge.sql")"
  bq --project_id="${PROJECT}" mk --transfer_config \
    --data_source=scheduled_query \
    --display_name="${name}" \
    --location="${REGION}" \
    --schedule="every 1 hours" \
    --service_account_name="${EXPORTER_SA}" \
    --params="$(python3 -c 'import json,sys; print(json.dumps({"query": sys.stdin.read()}))' <<<"${query}")"
}

verify() {
  log "Acceptance query (docs/23 §7.1) on native table"
  bq --project_id="${PROJECT}" query --use_legacy_sql=false --format=sparse \
    "SELECT JSON_VALUE(payload,'\$.windowsBuild') build, COUNT(*) c
       FROM \`${PROJECT}.dataminer_logs_${ENV}.client_events\`
      WHERE kind='preflight_failed'
        AND JSON_VALUE(payload,'\$.failureCode')='HARDWARE_PATH_PREFLIGHT_FAILED'
      GROUP BY build"
}

case "${2:-tables}" in
  tables)   tables ;;
  schedule) schedule ;;
  verify)   verify ;;
  all)      tables; schedule ;;
  *) echo "usage: $0 <env> {tables|schedule|verify|all}" >&2; exit 2 ;;
esac
