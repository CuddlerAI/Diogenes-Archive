"""S5: revocation drill. Simulate revoking one session -> build v2026.07.1 that
excludes it, using server-side CopyObject for every unaffected shard (no other
session's bytes are re-uploaded or touched). Nothing is deleted from v2026.07."""
import boto3, os, json, hashlib, datetime, sys
from botocore.config import Config

SCRATCH = "/private/tmp/claude-501/-Users-oratis-Documents-Diogenes--claude-worktrees-modest-taussig-cdac36/a1d262fc-eaf0-429f-9d25-10d579aed992/scratchpad"
OLD = "method-validation/cod-mv/v2026.07"
NEW = "method-validation/cod-mv/v2026.07.1"
BUCKET = "diogenes-training-dev"
REVOKED = "dev-1784973599178-0000"  # drill target: first train session

cfg = Config(signature_version="s3v4", region_name="auto", retries={"max_attempts": 8})
dev = boto3.client("s3", endpoint_url=os.environ["RCLONE_CONFIG_R2DEV_ENDPOINT"],
    aws_access_key_id=os.environ["RCLONE_CONFIG_R2DEV_ACCESS_KEY_ID"],
    aws_secret_access_key=os.environ["RCLONE_CONFIG_R2DEV_SECRET_ACCESS_KEY"], config=cfg)

def rt(fn, *a, **kw):
    """retry any boto3 call across the flaky local link"""
    import time
    last = None
    for i in range(8):
        try:
            return fn(*a, **kw)
        except Exception as e:
            last = e; print(f"  rt retry {i+1}: {type(e).__name__}"); time.sleep(min(2**i, 20))
    raise last

manifest = json.loads(rt(dev.get_object, Bucket=BUCKET, Key=f"{OLD}/MANIFEST.json")["Body"].read())
shards = json.loads(rt(dev.get_object, Bucket=BUCKET, Key=f"{OLD}/index/shards.json")["Body"].read())["shards"]

keep = [s for s in shards if s["session_id"] != REVOKED]
dropped = [s for s in shards if s["session_id"] == REVOKED]
assert dropped, "revoked session has no shards?"
print(f"revoking {REVOKED}: dropping {len(dropped)} shard(s), keeping {len(keep)}")

copied = 0
for s in keep:
    rt(dev.copy_object, Bucket=BUCKET, Key=f"{NEW}/{s['key']}",
       CopySource={"Bucket": BUCKET, "Key": f"{OLD}/{s['key']}"})
    copied += 1
# per-session index files for kept sessions (small, server-side copy too)
kept_sids = sorted({s["session_id"] for s in keep})
for sid in kept_sids:
    prefix = f"{OLD}/index/sessions/{sid}/"
    token = None
    while True:
        kw = dict(Bucket=BUCKET, Prefix=prefix, MaxKeys=1000)
        if token: kw["ContinuationToken"] = token
        r = rt(dev.list_objects_v2, **kw)
        for o in r.get("Contents", []):
            rel = o["Key"][len(OLD) + 1:]
            rt(dev.copy_object, Bucket=BUCKET, Key=f"{NEW}/{rel}",
               CopySource={"Bucket": BUCKET, "Key": o["Key"]})
            copied += 1
        if not r.get("IsTruncated"): break
        token = r["NextContinuationToken"]
print("server-side copies:", copied)

now = datetime.datetime.now(datetime.timezone.utc).isoformat()
new_manifest = dict(manifest)
new_manifest["version"] = "v2026.07.1"
new_manifest["created_utc"] = now
new_manifest["parent_version"] = "v2026.07"
new_manifest["revocation"] = {
    "revoked_session": REVOKED, "reason": "S5 revocation drill (docs/12 P9 scaled down)",
    "dropped_shards": [s["key"] for s in dropped],
    "mechanism": "per-session shards -> only the revoked session's shards disappear; every other shard is a server-side copy, bytes untouched",
    "note": "v2026.07 is left intact for diffing; a real revocation would also purge the revoked bytes from every version (requires explicit deletion approval)",
}
new_manifest["sessions"] = [s for s in manifest["sessions"] if s["session_id"] != REVOKED]
new_manifest["totals"] = {"sessions": len(new_manifest["sessions"]),
    "chunks": sum(s["chunk_count"] for s in new_manifest["sessions"]),
    "shards": len(keep), "shard_bytes": sum(s["bytes"] for s in keep)}

new_shards_doc = json.dumps({"schema": "dm-mv-shards-1", "shards": keep}, indent=2).encode()
new_manifest["index_files"] = {"index/shards.json": {"sha256": hashlib.sha256(new_shards_doc).hexdigest(), "bytes": len(new_shards_doc)}}
rt(dev.put_object, Bucket=BUCKET, Key=f"{NEW}/index/shards.json", Body=new_shards_doc)
rt(dev.put_object, Bucket=BUCKET, Key=f"{NEW}/MANIFEST.json", Body=json.dumps(new_manifest, indent=2).encode())

# verify: new version listing contains no revoked-session object; kept shard count matches
listed = []
token = None
while True:
    kw = dict(Bucket=BUCKET, Prefix=f"{NEW}/", MaxKeys=1000)
    if token: kw["ContinuationToken"] = token
    r = dev.list_objects_v2(**kw)
    listed += [o["Key"] for o in r.get("Contents", [])]
    if not r.get("IsTruncated"): break
    token = r["NextContinuationToken"]
leaked = [k for k in listed if REVOKED in k]
tars = [k for k in listed if k.endswith(".tar")]
print("new version objects:", len(listed), "tars:", len(tars), "revoked leakage:", len(leaked))
assert not leaked, f"revoked session leaked into new version: {leaked[:3]}"
assert len(tars) == len(keep), "kept shard count mismatch"
# spot-check: server-side copies must preserve bytes -> ETag(old) == ETag(new) for every kept shard
for s in keep:
    old_etag = rt(dev.head_object, Bucket=BUCKET, Key=f"{OLD}/{s['key']}")["ETag"]
    new_etag = rt(dev.head_object, Bucket=BUCKET, Key=f"{NEW}/{s['key']}")["ETag"]
    assert old_etag == new_etag, f"ETag drift on {s['key']}: {old_etag} != {new_etag}"
print("PASS all", len(keep), "copied shards preserve ETag (bytes identical, zero data transfer)")
print("S5 DRILL OK: v2026.07.1 =", len(keep), "shards, revoked session absent, parent untouched")
