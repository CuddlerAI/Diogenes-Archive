#!/usr/bin/env bash
#
# Post-deploy verification for the DataMiner production stack (docs/25 P1h,
# docs/21 §4). READ-ONLY: it describes services and probes health/gates, and
# never creates, changes, or deploys anything.
#
# It handles NO secrets. Checks that need a credential (R2 keys, the DB URL, the
# console proxy token) are PRINTED as operator commands rather than run — pass the
# console proxy token via CONSOLE_PROXY_TOKEN in the environment ONLY if you want
# this script to run the two curl probes for you; it is never printed.
#
#   bash DataMiner/tools/verify-production.sh
#   CONSOLE_PROXY_TOKEN=... bash DataMiner/tools/verify-production.sh   # also runs the curl probes
#
# Exit 0 = every check this script could run passed. Non-zero = a check failed or
# a required service is missing. Manual/operator checks are reported, not counted.
set -uo pipefail

readonly PROJECT="hakkolite"
readonly REGION="us-central1"
readonly CONSOLE_DOMAIN="console.diogenesmodel.com"
readonly API_SERVICE="dataminer-api-production"
readonly WORKER_SERVICE="dataminer-api-worker-production"
readonly CONSOLE_SERVICE="dataminer-console-api-production"
readonly PROD_PREFIX="raw/v1/non-trainable/"

pass() { printf '  \033[1;32m✔\033[0m %s\n' "$*"; }
fail() { printf '  \033[1;31m✗\033[0m %s\n' "$*"; FAILED=1; }
info() { printf '  \033[1;34mℹ\033[0m %s\n' "$*"; }
todo() { printf '  \033[1;33m→ operator\033[0m %s\n' "$*"; }
section() { printf '\n\033[1m%s\033[0m\n' "$*"; }
FAILED=0

command -v gcloud >/dev/null || { echo "gcloud not found"; exit 2; }

# Preflight. Without this, an expired token makes every `describe` below fail and
# the script reports "is not deployed" for a perfectly healthy production — the
# most alarming possible way to say "please log in". Fail loudly and early instead:
# an unreachable control plane means this run proves nothing, not that prod is gone.
preflight_error="$(gcloud projects describe "$PROJECT" --format="value(projectId)" 2>&1 >/dev/null)"
if [[ -n "$preflight_error" ]]; then
  printf '\033[1;31mcannot reach project %s — this run would report false failures, so it is aborting.\033[0m\n\n' "$PROJECT"
  printf '%s\n\n' "$preflight_error"
  printf 'Most likely: the auth token expired. Fix, then re-run:\n'
  printf '  gcloud auth login\n'
  printf '  gcloud config set account <the account with %s access>\n' "$PROJECT"
  exit 2
fi

# service_url NAME — the Cloud Run URL, or empty when we could not get one.
# Only used where empty already means "skip this probe"; section 1 does its own
# three-way classification inline, because there the distinction is the point.
service_url() {
  gcloud run services describe "$1" --project "$PROJECT" --region "$REGION" \
    --format="value(status.url)" 2>/dev/null
}

# Three outcomes, deliberately kept apart — collapsing the last two into "empty"
# is exactly what let an expired token be reported as "production is not deployed":
#   describe succeeds with a URL  → deployed
#   describe says NOT_FOUND       → genuinely not deployed
#   describe fails any other way  → we learned nothing; say so, don't guess
#
# Classified inline rather than in a helper: a helper's error text would have to
# come back through a global, and `x="$(helper)"` runs the helper in a subshell,
# so that global never reaches the caller. (It didn't — first version shipped an
# empty error message.)
section "1 · Cloud Run services exist and are serving"
for svc in "$CONSOLE_SERVICE" "$API_SERVICE" "$WORKER_SERVICE"; do
  if out="$(gcloud run services describe "$svc" --project "$PROJECT" --region "$REGION" \
      --format="value(status.url)" 2>&1)"; then
    if [[ -n "$out" ]]; then pass "$svc → $out"; else fail "$svc is not deployed"; fi
  elif grep -qiE 'not found|NOT_FOUND' <<<"$out"; then
    fail "$svc is not deployed"
  else
    fail "$svc — could not ask Cloud Run, so this is NOT a deployment verdict: ${out%%$'\n'*}"
  fi
done
API_URL="$(service_url "$API_SERVICE")"
CONSOLE_URL="$(service_url "$CONSOLE_SERVICE")"

section "2 · proxy-token gate (docs/11 §4.3, #67): direct Cloud Run hit must 404"
# No secret needed: the WHOLE point is that a caller WITHOUT the token is refused.
# A production console that answers /console/v1/* from a raw Cloud Run URL is the
# #33 exposure; it must return 404 (not 401 — the route must look absent).
if [[ -n "$CONSOLE_URL" ]]; then
  code="$(curl -s -o /dev/null -w '%{http_code}' "$CONSOLE_URL/console/v1/auth/session" || echo 000)"
  if [[ "$code" == "404" ]]; then pass "unproxied /console/v1 → 404 (gate active)"
  elif [[ "$code" == "000" ]]; then info "could not reach $CONSOLE_URL (IAM may block direct hits — also acceptable)"
  else fail "unproxied /console/v1 → $code (expected 404; gate not active?)"; fi
else
  info "console URL unavailable — skipping gate probe (see section 1 for why)"
fi

section "3 · health via the console domain (needs the proxy token)"
if [[ -n "${CONSOLE_PROXY_TOKEN:-}" ]]; then
  code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "x-console-proxy-token: $CONSOLE_PROXY_TOKEN" "https://$CONSOLE_DOMAIN/readyz" || echo 000)"
  if [[ "$code" == "200" ]]; then pass "https://$CONSOLE_DOMAIN/readyz → 200"
  else fail "https://$CONSOLE_DOMAIN/readyz → $code (expected 200)"; fi
else
  todo "curl -H 'x-console-proxy-token: <token>' https://$CONSOLE_DOMAIN/readyz   # expect 200"
fi

section "4 · ingestion API health"
if [[ -n "$API_URL" ]]; then
  # /readyz is unauthenticated (Cloud Run probe path); if IAM blocks it, run from
  # an authorized context. /v1/* is per-device authed, not probed here.
  code="$(curl -s -o /dev/null -w '%{http_code}' "$API_URL/readyz" || echo 000)"
  if [[ "$code" == "200" ]]; then pass "$API_URL/readyz → 200"
  elif [[ "$code" == "000" || "$code" == "403" ]]; then info "readyz not reachable unauthenticated ($code) — probe from an authorized context"
  else fail "$API_URL/readyz → $code"; fi
else
  info "ingestion API URL unavailable — skipping (see section 1 for why)"
fi

section "5 · secret-gated checks (operator runs — this script holds no secrets)"
todo "R2 write path:   cd DataMiner/apps/api && R2_ACCOUNT_ID=… R2_BUCKET=diogenes-model-training R2_ACCESS_KEY_ID=… R2_SECRET_ACCESS_KEY=… pnpm check:r2"
todo "object prefix:   confirm written keys live under '$PROD_PREFIX' (no bytes outside it)"
todo "prod≠trainable:  psql \"\$DATABASE_URL\" -c 'SELECT count(*) FROM clip_trainability WHERE is_trainable'   # expect 0 until the grant chain (docs/25 P2) lands"

# --- 6 · three-plane reconciliation (docs/54) ---------------------------------
#
# Catalog truth lives in three places that nothing reconciles automatically:
#   P-repo    config/catalogs/**  (+ the Rust include_bytes! surface)
#   P-prod-db catalog_versions / catalog_releases
#   P-box     whatever the collection machine's client actually loaded
#
# docs/54 found six profiles on a collection box that exist in no git ref. This
# section prints the repo baseline so the other two planes can be compared
# against it, and prints (never runs) the DB queries that need a credential.
section "6 · three-plane reconciliation baseline (docs/54)"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if command -v python3 >/dev/null; then
  python3 - "$repo_root" <<'PY'
import json, os, sys
root = os.path.join(sys.argv[1], "config", "catalogs")
rows = []
for lane in ("dev", "release", "certified"):
    lane_dir = os.path.join(root, lane)
    if not os.path.isdir(lane_dir):
        continue
    for name in sorted(os.listdir(lane_dir)):
        gp = os.path.join(lane_dir, name, "game-profile.json")
        if not os.path.isfile(gp):
            continue
        try:
            g = json.load(open(gp, encoding="utf-8"))
        except (OSError, ValueError) as exc:
            rows.append((lane, name, f"<unreadable: {exc}>", "", "", ""))
            continue
        rows.append((
            lane, name,
            str(g.get("game_id", "?")), str(g.get("version", "?")),
            "exe:pin" if g.get("executable_sha256") else "exe:name-only",
            "win:pin" if g.get("window_class") else "win:none",
        ))
if not rows:
    print("  no game profiles found under config/catalogs — is this the repo root?")
    sys.exit(0)
w = max(len(r[2]) for r in rows)
for lane, name, gid, ver, exe, win in rows:
    print(f"  {lane:<9} {gid:<{w}}  v{ver:<3} {exe:<13} {win}")
print(f"\n  {len(rows)} game profiles in the repo plane.")
PY
  info "compare the above against the two planes below; any row present in one plane only is drift"
else
  info "python3 not found — skipping the repo baseline print"
fi

# Deployment gates that are readable without any secret: we ask for env var
# NAMES only. A missing name here is a silent fail-closed that docs/54 §F4 hit —
# the trainable-grant form 422s with policy_blocked and nothing else says why.
section "7 · deployment gates (env var names only — no values read)"
env_names() {
  local out
  out="$(gcloud run services describe "$1" --project "$PROJECT" --region "$REGION" \
    --format="value(spec.template.spec.containers[0].env[].name)" 2>&1)" || return 1
  tr ';' '\n' <<<"$out"
}
check_env() { # check_env SERVICE VAR_NAME DOC
  local svc="$1" var="$2" doc="$3"
  local names
  if ! names="$(env_names "$svc")"; then
    info "$svc — could not ask Cloud Run, so $var is UNKNOWN (not 'missing')"
    return
  fi
  if [[ -z "$names" ]]; then
    info "$svc declares no env vars at all — cannot check $var"
  elif grep -qx "$var" <<<"$names"; then
    pass "$svc has $var  ($doc)"
  else
    fail "$svc is MISSING $var  ($doc)"
  fi
}
check_env "$API_SERVICE"     "R2_PREFIX_FIRST_PARTY"              "docs/31 §1b — unset means the first-party lane is not admitted"
check_env "$CONSOLE_SERVICE" "CONSOLE_APP_TERMS_ALLOWS_TRAINABLE" "docs/31 §1c — consent may grant trainable use"
check_env "$CONSOLE_SERVICE" "CONSOLE_GOVERNANCE_ALLOW_TRAINABLE" "docs/31 §1c — operator may issue trainable grants"

section "8 · reconciliation checks the operator must run (need a credential)"
todo "prod-db roster:  psql \"\$DATABASE_URL\" -c 'SELECT kind, catalog_id, version, game_id, approved, approved_for_trainable, revoked_at FROM catalog_versions ORDER BY kind, catalog_id, version;'"
todo "prod-db release: psql \"\$DATABASE_URL\" -c 'SELECT id, channel, catalog_id, catalog_version, published_at, is_current FROM catalog_releases ORDER BY published_at DESC LIMIT 10;'"
todo "collection box:  read the client's 目录代际 (active/cached generation) and export diagnostics; its catalog identity must match the section 6 baseline"
todo "drift verdict:   any game the box can select that is not in section 6 is an untracked bundle — freeze it (docs/54 G-1) before capturing"

section "Summary"
if [[ "$FAILED" -eq 0 ]]; then
  printf '  \033[1;32mautomated checks passed\033[0m — complete the operator-run checks in section 5.\n'
  exit 0
else
  printf '  \033[1;31msome automated checks failed\033[0m — see above.\n'
  exit 1
fi
