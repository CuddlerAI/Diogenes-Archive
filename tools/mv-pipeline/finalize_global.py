"""Mac-side global finalize: merge per-session indexes -> shards.json / parquet /
MANIFEST / PROVENANCE, upload small global artifacts to dev bucket."""
import boto3, os, json, hashlib, gzip, io, sys, datetime
from botocore.config import Config
from boto3.s3.transfer import TransferConfig

SCRATCH = "/private/tmp/claude-501/-Users-oratis-Documents-Diogenes--claude-worktrees-modest-taussig-cdac36/a1d262fc-eaf0-429f-9d25-10d579aed992/scratchpad"
BASE = "method-validation/cod-mv/v2026.07"
BUCKET = "diogenes-training-dev"
COMMIT = "2a6ffd2809801c876c2de286c066b8cc34e5a001"
FAULTED = {"dev-1785039658512-0000": "CAPTURE_QUEUE_DROPPED", "dev-1785045787023-0005": "INPUT_NOT_NEUTRAL"}

cfg = Config(signature_version="s3v4", region_name="auto", retries={"max_attempts": 8})
dev = boto3.client("s3", endpoint_url=os.environ["RCLONE_CONFIG_R2DEV_ENDPOINT"],
    aws_access_key_id=os.environ["RCLONE_CONFIG_R2DEV_ACCESS_KEY_ID"],
    aws_secret_access_key=os.environ["RCLONE_CONFIG_R2DEV_SECRET_ACCESS_KEY"], config=cfg)
tc = TransferConfig(multipart_threshold=128*1024, multipart_chunksize=128*1024, max_concurrency=16)

def list_keys(prefix):
    keys, token = [], None
    while True:
        kw = dict(Bucket=BUCKET, Prefix=prefix, MaxKeys=1000)
        if token: kw["ContinuationToken"] = token
        r = dev.list_objects_v2(**kw)
        keys += [(o["Key"], o["Size"]) for o in r.get("Contents", [])]
        if not r.get("IsTruncated"): break
        token = r["NextContinuationToken"]
    return keys

def get(key):
    last = None
    for attempt in range(6):
        try:
            return dev.get_object(Bucket=BUCKET, Key=key)["Body"].read()
        except Exception as e:  # flaky local<->R2 link: retry whole object
            last = e
            print(f"  retry {attempt+1} for {key}: {type(e).__name__}")
    raise last

os.makedirs(f"{SCRATCH}/global", exist_ok=True)
summary = json.loads(get(f"{BASE}/logs/final-summary.json"))
sessions_idx = {}
all_keys = list_keys(f"{BASE}/index/sessions/")
for key, size in all_keys:
    if key.endswith("/session-index.json"):
        sid = key.split("/")[-2]
        sessions_idx[sid] = json.loads(get(key))
print("session indexes:", len(sessions_idx))

# bucket-side shard listing for cross-check
shard_objects = {k: s for k, s in list_keys(f"{BASE}/train/") + list_keys(f"{BASE}/test/")}

shards_json, samples_rows, framepairs_rows = [], [], []
mismatch = []
for sid, idx in sorted(sessions_idx.items()):
    for sh in idx["shards"]:
        key = f"{BASE}/{idx['split']}/{sid}/{sh['name']}"
        listed = shard_objects.get(key)
        if listed != sh["bytes"]:
            mismatch.append((key, sh["bytes"], listed))
        shards_json.append({"session_id": sid, "split": idx["split"], "key": f"{idx['split']}/{sid}/{sh['name']}",
                            "bytes": sh["bytes"], "sha256": sh["sha256"], "entry_count": sh["entry_count"]})
    for line in get(f"{BASE}/index/sessions/{sid}/samples.jsonl").decode().splitlines():
        if line.strip(): samples_rows.append(json.loads(line))
    fp = get(f"{BASE}/index/sessions/{sid}/framepairs.jsonl").decode()
    for line in fp.splitlines():
        if line.strip():
            row = json.loads(line)
            row["session_id"] = sid
            framepairs_rows.append(row)
if mismatch:
    print("SIZE MISMATCH:", mismatch); sys.exit(1)
print("shards:", len(shards_json), "samples:", len(samples_rows), "framepairs:", len(framepairs_rows))

import pyarrow as pa, pyarrow.parquet as pq
def rows_to_parquet(rows, path):
    flat = [{k: (json.dumps(v) if isinstance(v, (dict, list)) else v) for k, v in r.items()} for r in rows]
    table = pa.Table.from_pylist(flat)
    pq.write_table(table, path)
rows_to_parquet(samples_rows, f"{SCRATCH}/global/samples.parquet")
rows_to_parquet(framepairs_rows, f"{SCRATCH}/global/framepairs.parquet")

with open(f"{SCRATCH}/global/shards.json", "w") as f:
    json.dump({"schema": "dm-mv-shards-1", "shards": shards_json}, f, indent=2)

def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""): h.update(chunk)
    return h.hexdigest()

now = datetime.datetime.now(datetime.timezone.utc).isoformat()
per_session = {r["session"]: r for r in summary["summary"]}
manifest = {
    "schema": "dm-mv-manifest-1",
    "dataset": "cod-mv", "version": "v2026.07",
    "is_trainable": False, "method_validation_only": True,
    "created_utc": now,
    "source": {
        "bucket": "diogenes-model-training",
        "prefix": "raw/v1/non-trainable/manual/DESKTOP-3PG7OEU/com.diogeneslab.dataminer/non-trainable-sessions/",
        "raw_objects": 4802, "raw_bytes": 18252173986,
    },
    "converter": {"tool": "dataminer-data-demo convert --profile local-debug",
                   "repo_commit": COMMIT,
                   "determinism_check": next((r.get("determinism_identical") for r in summary["summary"] if "determinism_identical" in r), None)},
    "split_rule": "completed sessions sorted by id ascending; last 2 -> test; split by session (no chunk-level mixing)",
    "sessions": [{"session_id": sid, "split": idx["split"], "chunk_count": idx["chunk_count"],
                   "shards": idx["shards"], "seconds": per_session.get(sid, {}).get("seconds")}
                  for sid, idx in sorted(sessions_idx.items())],
    "excluded_sessions": [{"session_id": sid, "reason": reason,
                            "converter_rejected": per_session.get(sid, {}).get("failed_as_expected")}
                           for sid, reason in sorted(FAULTED.items())],
    "totals": {"sessions": len(sessions_idx),
                "chunks": sum(i["chunk_count"] for i in sessions_idx.values()),
                "shards": len(shards_json),
                "shard_bytes": sum(s["bytes"] for s in shards_json)},
    "index_files": {},
}
for name in ["shards.json", "samples.parquet", "framepairs.parquet"]:
    manifest["index_files"][f"index/{name}"] = {"sha256": sha256_file(f"{SCRATCH}/global/{name}"),
                                                 "bytes": os.path.getsize(f"{SCRATCH}/global/{name}")}
ds_path = f"{SCRATCH}/DATASHEET.md"
manifest["index_files"]["DATASHEET.md"] = {"sha256": sha256_file(ds_path), "bytes": os.path.getsize(ds_path)}

provenance = {
    "schema": "dm-mv-provenance-1",
    "declaration": "OUT-OF-BAND materialization. No upload_jobs or server-side registration was written; the ingest pipeline (docs/09/12 P1-P8) was NOT used. This is a method-validation artifact per model_research/research/20 §6.2.",
    "source_machine_mirror": "DESKTOP-3PG7OEU (collection machine data-root mirror, out-of-band synced into diogenes-model-training by the collection operator)",
    "journal_facts": {"completed_sessions": 18, "faulted_sessions": FAULTED,
                       "data_model_jobs": {"completed": 18, "waiting": 2,
                       "waiting_explanation": "the 2 waiting jobs belong to the 2 faulted sessions; they are correctly unprocessed, not stuck"}},
    "conversion_host": "hakko-win-gcp (GCP us-central1-a, no GPU; conversion is CPU-only RAW replay)",
    "repo_commit": COMMIT,
    "pipeline": {"driver_sha256": sha256_file(f"{SCRATCH}/mv_pipeline_driver.mjs"),
                  "assembler_sha256": sha256_file("/Users/example/Documents/Diogenes/.claude/worktrees/modest-taussig-cdac36/tools/assemble_mv_dataset.mjs"),
                  "transport": "presigned GET (source, read-only) / presigned PUT (dev bucket); no long-lived credentials on the VM"},
    "red_lines": ["source bucket accessed read-only end to end",
                   "consent chain empty and non-backfillable -> permanently non-trainable",
                   "all planes stamped method_validation_only"],
    "created_utc": now,
    "operator": "Claude Code (approved plan: model_research/research/20-raw-to-dataset-method-validation-pipeline.md)",
}

with open(f"{SCRATCH}/global/MANIFEST.json", "w") as f: json.dump(manifest, f, indent=2)
with open(f"{SCRATCH}/global/PROVENANCE.json", "w") as f: json.dump(provenance, f, indent=2)

uploads = {f"{BASE}/index/shards.json": f"{SCRATCH}/global/shards.json",
           f"{BASE}/index/samples.parquet": f"{SCRATCH}/global/samples.parquet",
           f"{BASE}/index/framepairs.parquet": f"{SCRATCH}/global/framepairs.parquet",
           f"{BASE}/MANIFEST.json": f"{SCRATCH}/global/MANIFEST.json",
           f"{BASE}/PROVENANCE.json": f"{SCRATCH}/global/PROVENANCE.json",
           f"{BASE}/DATASHEET.md": ds_path}
for key, path in uploads.items():
    dev.upload_file(path, BUCKET, key, Config=tc)
    print("uploaded", key, os.path.getsize(path))
print("GLOBAL FINALIZE OK")
