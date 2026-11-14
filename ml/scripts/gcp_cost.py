#!/usr/bin/env python3
"""Report GCP cost for the training VMs, from measured uptime.

Audit logs are NOT a reliable basis: a capacity-hunting loop that retries
`instances.start` every few minutes logs an entry per attempt, including the
ones that fail with ZONE_RESOURCE_POOL_EXHAUSTED. Pairing the first such
entry with the eventual stop inflated one VM from 1.4 h to 52 h — a 38x
overstatement. Cloud Monitoring's instance/uptime is what actually billed.

    python3 ml/scripts/gcp_cost.py --project hakkolite --prefix diogenes-vl
    python3 ml/scripts/gcp_cost.py --days 2      # this round only
"""
from __future__ import annotations

import argparse
import json
import subprocess
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone

# us-region list prices, USD/hour. Spot is billed at roughly 40% of these.
PRICE_PER_HOUR = {
    "g2-standard-8": 0.85,     # 1x L4
    "g2-standard-16": 1.07,    # 1x L4, more vCPU
    "g2-standard-48": 3.71,    # 4x L4
    "g2-standard-96": 7.42,    # 8x L4
    "a2-highgpu-1g": 3.67,     # 1x A100-40
    "a2-highgpu-2g": 7.35,     # 2x A100-40
    "a2-highgpu-4g": 14.69,    # 4x A100-40
    "n2-standard-64": 3.11,    # CPU data prep, 2x the -32 (linear in vCPU+RAM)
    "n2-standard-32": 1.55,    # CPU data prep
    "n2-standard-16": 0.78,
    "n2-standard-8": 0.39,
}
SPOT_FACTOR = 0.4


def _token() -> str:
    return subprocess.run(["gcloud", "auth", "print-access-token"],
                          capture_output=True, text=True, check=True).stdout.strip()


def uptime_hours(project: str, days: float) -> dict[str, float]:
    """Actual billed running hours per instance name."""
    end = datetime.now(timezone.utc)
    start = end - timedelta(days=days)
    params = {
        "filter": ('metric.type="compute.googleapis.com/instance/uptime" '
                   'AND resource.type="gce_instance"'),
        "interval.startTime": start.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "interval.endTime": end.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "aggregation.alignmentPeriod": "3600s",
        "aggregation.perSeriesAligner": "ALIGN_SUM",
        "aggregation.crossSeriesReducer": "REDUCE_SUM",
        "aggregation.groupByFields": "metadata.system_labels.name",
    }
    url = (f"https://monitoring.googleapis.com/v3/projects/{project}"
           f"/timeSeries?{urllib.parse.urlencode(params)}")
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {_token()}"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = json.load(resp)
    out: dict[str, float] = {}
    for s in data.get("timeSeries", []):
        sl = s.get("metadata", {}).get("systemLabels", {}) or {}
        name = sl.get("name")
        if not name:
            continue
        secs = sum(float(p["value"].get("doubleValue", 0)) for p in s.get("points", []))
        out[name] = out.get(name, 0.0) + secs / 3600.0
    return out


def instance_specs(project: str, prefix: str, days: float) -> dict[str, tuple[str, bool]]:
    """machine type + spot flag per instance, live AND deleted.

    Live instances come from `instances list`; deleted ones are recovered
    from the audit log's instances.insert request payload, so a short-lived
    data-prep VM still shows up in the bill instead of silently vanishing.
    """
    fmt = "value(name,machineType.basename(),scheduling.provisioningModel)"
    r = subprocess.run(["gcloud", "compute", "instances", "list",
                        "--project", project, "--format", fmt],
                       capture_output=True, text=True)
    specs: dict[str, tuple[str, bool]] = {}
    for line in r.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0].startswith(prefix):
            spot = len(parts) > 2 and parts[2].upper() == "SPOT"
            specs[parts[0]] = (parts[1], spot)

    log = subprocess.run(
        ["gcloud", "logging", "read",
         'protoPayload.methodName:"instances.insert"',
         "--project", project, "--limit", "500",
         f"--freshness={int(days) + 1}d", "--format", "json"],
        capture_output=True, text=True)
    try:
        entries = json.loads(log.stdout or "[]")
    except json.JSONDecodeError:
        entries = []
    for e in entries:
        pp = e.get("protoPayload", {})
        name = (pp.get("resourceName", "") or "").rsplit("/", 1)[-1]
        if not name.startswith(prefix) or name in specs:
            continue
        req = pp.get("request", {}) or {}
        mt = (req.get("machineType", "") or "").rsplit("/", 1)[-1]
        spot = str(req.get("scheduling", {}).get("provisioningModel", "")).upper() == "SPOT"
        if mt:
            specs[name] = (mt, spot)
    return specs


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--project", default="hakkolite")
    ap.add_argument("--prefix", default="diogenes-vl")
    ap.add_argument("--days", type=float, default=5.0)
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    hours = uptime_hours(args.project, args.days)
    specs = instance_specs(args.project, args.prefix, args.days)

    rows, total, unknown = [], 0.0, []
    for name, h in hours.items():
        if not name.startswith(args.prefix):
            continue
        mach, spot = specs.get(name, (None, False))
        if mach is None or mach not in PRICE_PER_HOUR:
            unknown.append((name, mach, h))
            continue
        rate = PRICE_PER_HOUR[mach] * (SPOT_FACTOR if spot else 1.0)
        cost = h * rate
        total += cost
        rows.append({"instance": name, "machine": mach,
                     "billing": "spot" if spot else "on-demand",
                     "hours": round(h, 2), "usd_per_hour": round(rate, 2),
                     "cost_usd": round(cost, 2)})
    rows.sort(key=lambda r: -r["cost_usd"])

    if args.json:
        print(json.dumps({"rows": rows, "total_usd": round(total, 2),
                          "window_days": args.days,
                          "unpriced": unknown}, indent=2))
        return

    print(f"{'instance':26}{'machine':18}{'billing':11}{'hours':>8}{'$/h':>8}{'cost':>9}")
    print("-" * 80)
    for r in rows:
        print(f"{r['instance']:26}{r['machine']:18}{r['billing']:11}"
              f"{r['hours']:8.2f}{r['usd_per_hour']:8.2f}{r['cost_usd']:9.2f}")
    print("-" * 80)
    print(f"{'TOTAL (last %.1f days)' % args.days:63}{total:17.2f}")
    if unknown:
        print("\nno price on file (deleted VM or unlisted type) — add to "
              "PRICE_PER_HOUR to include:")
        for n, m, h in unknown:
            print(f"  {n}  machine={m}  {h:.2f} h")


if __name__ == "__main__":
    main()
