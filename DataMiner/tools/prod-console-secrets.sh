#!/usr/bin/env bash
#
# prod-console-secrets.sh — create the 9 Secret Manager secrets that
# tools/deploy-console-production.sh references via `--set-secrets ...:latest`
# for dataminer-console-api-production. None of them exist yet (verified
# 2026-07-25). This is the "prepare the secrets" step of docs/21 §2/§3.
#
# DRY-RUN BY DEFAULT: prints the exact gcloud commands and which secrets already
# exist, and creates NOTHING. Pass --apply to create; under --apply each value
# is read from the matching environment variable (never hard-coded, never echoed).
#
#   # see the plan + what's missing:
#   bash tools/prod-console-secrets.sh
#
#   # create them (fill every value first; see constraints below):
#   CONSOLE_DB_URL=... APP_DB_URL=... IP_HASH_SALT=... PROXY_TOKEN=... \
#   MAIL_API_KEY=... TERMS_HASH=... R2_ACCOUNT_ID=... R2_ACCESS_KEY_ID=... \
#   R2_SECRET_ACCESS_KEY=... \
#   bash tools/prod-console-secrets.sh --apply
#
set -euo pipefail

readonly PROJECT="hakkolite"
readonly INSTANCE_CONN="hakkolite:us-central1:dataminer-db-production"
readonly DB="dataminer_production"

APPLY=0
[[ "${1:-}" == "--apply" ]] && APPLY=1
[[ "${1:-}" == "-h" || "${1:-}" == "--help" ]] && { sed -n '2,20p' "$0"; exit 0; }

log()  { printf '\033[1;34m▸\033[0m %s\n' "$*"; }
note() { printf '   %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }

# secret <secret-name> <env-var-holding-value> <constraint description>
secret() {
  local name="$1" env="$2" desc="$3"
  if gcloud secrets describe "$name" --project "$PROJECT" >/dev/null 2>&1; then
    printf '\033[1;32m✓\033[0m %-38s already exists — skip\n' "$name"
    return
  fi
  printf '\033[1;33m✗\033[0m %-38s MISSING  (value ← $%s)\n' "$name" "$env"
  note "constraint: $desc"
  if [[ "$APPLY" -eq 1 ]]; then
    local val="${!env:-}"
    [[ -n "$val" ]] || { warn "set \$$env before --apply"; exit 1; }
    log "creating $name"
    printf '%s' "$val" | gcloud secrets create "$name" \
      --project "$PROJECT" --replication-policy=automatic --data-file=-
  else
    note "would run: printf '%s' \"\$$env\" | gcloud secrets create $name --project $PROJECT --replication-policy=automatic --data-file=-"
  fi
  printf '\n'
}

log "prod console secrets — project $PROJECT  (mode: $([[ $APPLY -eq 1 ]] && echo APPLY || echo DRY-RUN))"
printf '\n'

# 1–2 · DB connection strings (Cloud SQL unix socket; two DISTINCT roles, one DB).
#        Format: postgresql://<role>:<pw>@/${DB}?host=/cloudsql/${INSTANCE_CONN}
secret dataminer-console-prod-db-url  CONSOLE_DB_URL \
  "postgresql://dataminer_console:<pw>@/${DB}?host=/cloudsql/${INSTANCE_CONN} (governance role)"
secret dataminer-app-prod-db-url      APP_DB_URL \
  "postgresql://dataminer_app:<pw>@/${DB}?host=/cloudsql/${INSTANCE_CONN} (annotator-surface role; least-priv per apps/console-api/scripts/grant-console-roles.sql)"

# 3 · IP hash salt.
secret dataminer-console-prod-ip-salt CONSOLE_IP_HASH_SALT \
  ">=32 random bytes, independent of staging; not a 'replace-me' placeholder"

# 4 · Proxy token — MUST differ from staging's.
secret dataminer-console-prod-proxy-token PROXY_TOKEN \
  ">=32 bytes, MUST differ from staging's CONSOLE_PROXY_TOKEN; same value goes to the Worker: wrangler secret put CONSOLE_PROXY_TOKEN --env production"

# 5 · Resend production mail key. Sender must be @…diogenesmodel.com.
secret dataminer-console-prod-mail-key CONSOLE_MAIL_API_KEY \
  "Resend PRODUCTION api key (from MAIL_FROM=no-reply@mail.diogenesmodel.com)"

# 6 · Frozen consent terms hash (G3) — sha256 of the frozen text.
secret dataminer-app-prod-terms-hash  TERMS_HASH \
  "64-hex = sha256 of the raw CONSOLE_APP_TERMS_BODY_PATH file bytes (no trim); see tools/prod-consent-terms.md"

# 7–9 · R2 read-only preview credentials — separate from the ingest WRITE keys,
#        scoped to bucket diogenes-model-training, independently revocable.
secret dataminer-console-prod-r2-account CONSOLE_R2_ACCOUNT_ID   "Cloudflare R2 account id"
secret dataminer-console-prod-r2-key     CONSOLE_R2_ACCESS_KEY_ID "R2 access key id — READ-ONLY, NOT the ingest write key"
secret dataminer-console-prod-r2-secret  CONSOLE_R2_SECRET_ACCESS_KEY "R2 secret — read-only, revocable alone"

# 10–12 · R2 read-only REVIEW credentials (docs/33 §7.2, the M0 step). A THIRD
#         token, independent from both the ingest write keys and the preview
#         key: issued in the CF Dashboard as Object Read only, scoped to exactly
#         the two review buckets (diogenes-model-training + diogenes-training-dev),
#         revocable without touching preview. Opt-in via WITH_REVIEW=1 so runs
#         that only need secrets 1–9 do not have to supply these.
if [[ "${WITH_REVIEW:-0}" == "1" ]]; then
  secret dataminer-console-prod-r2-review-account REVIEW_R2_ACCOUNT_ID \
    "Cloudflare R2 account id (same account, its own secret so rotation stays independent)"
  secret dataminer-console-prod-r2-review-key     REVIEW_R2_ACCESS_KEY_ID \
    "R2 access key id — Object Read only, scoped to the two review buckets, NOT preview's key"
  secret dataminer-console-prod-r2-review-secret  REVIEW_R2_SECRET_ACCESS_KEY \
    "R2 secret for the review token — revocable alone"
else
  note "review secrets (10–12, docs/33) skipped — rerun with WITH_REVIEW=1 to include them"
fi

# 13 · HF read-only token (docs/33 §6). Fine-grained, READ ONLY, scoped to the
#      teaser org. Opt-in via WITH_HF=1.
if [[ "${WITH_HF:-0}" == "1" ]]; then
  secret dataminer-console-prod-hf-token HF_READ_TOKEN \
    "HuggingFace fine-grained READ-ONLY token for the repos in CONSOLE_HF_REPOS"
else
  note "hf token secret (13, docs/33) skipped — rerun with WITH_HF=1 to include it"
fi

log "after creating: grant the console-api-production runtime service account roles/secretmanager.secretAccessor on each secret"
note "(mirror the SA staging's dataminer-console-api uses, or read it post-first-deploy:"
note " gcloud run services describe dataminer-console-api-production --project $PROJECT --region us-central1 --format='value(spec.template.spec.serviceAccountName)')"
log "done — nothing was created in dry-run."
