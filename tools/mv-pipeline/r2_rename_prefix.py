"""Rename R2 prefix method-validation/cod-mv/ -> method-validation/mv-gameplay/
via server-side copies (bytes never leave R2), then delete the old prefix.
MANIFEST `dataset` field is rewritten to match. Information-preserving."""
import boto3, os, json, time
from botocore.config import Config
from concurrent.futures import ThreadPoolExecutor, as_completed

BUCKET = "diogenes-training-dev"
OLD = "method-validation/cod-mv/"
NEW = "method-validation/mv-gameplay/"
cfg = Config(signature_version="s3v4", region_name="auto", retries={"max_attempts": 10},
             max_pool_connections=32)

def client():
    return boto3.client("s3", endpoint_url=os.environ["RCLONE_CONFIG_R2DEV_ENDPOINT"],
        aws_access_key_id=os.environ["RCLONE_CONFIG_R2DEV_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["RCLONE_CONFIG_R2DEV_SECRET_ACCESS_KEY"], config=cfg)

dev = client()

def rt(fn, *a, **kw):
    last = None
    for i in range(8):
        try:
            return fn(*a, **kw)
        except Exception as e:
            last = e
            time.sleep(min(2 ** i, 15))
    raise last

def list_all(prefix):
    keys, token = [], None
    while True:
        kw = dict(Bucket=BUCKET, Prefix=prefix, MaxKeys=1000)
        if token:
            kw["ContinuationToken"] = token
        r = rt(dev.list_objects_v2, **kw)
        keys += [o["Key"] for o in r.get("Contents", [])]
        if not r.get("IsTruncated"):
            break
        token = r["NextContinuationToken"]
    return keys

src_keys = list_all(OLD)
print("objects to move:", len(src_keys), flush=True)

_local = {}
def worker_client():
    import threading
    tid = threading.get_ident()
    if tid not in _local:
        _local[tid] = client()
    return _local[tid]

def copy_one(key):
    c = worker_client()
    dst = NEW + key[len(OLD):]
    rt(c.copy_object, Bucket=BUCKET, Key=dst, CopySource={"Bucket": BUCKET, "Key": key})
    return dst

done = 0
with ThreadPoolExecutor(max_workers=12) as ex:
    futs = {ex.submit(copy_one, k): k for k in src_keys}
    for f in as_completed(futs):
        f.result()
        done += 1
        if done % 25 == 0:
            print(f"copied {done}/{len(src_keys)}", flush=True)
print("copied all:", done, flush=True)

# rewrite MANIFEST dataset name in both versions
for ver in ["v2026.07", "v2026.07.1"]:
    key = f"{NEW}{ver}/MANIFEST.json"
    try:
        body = rt(dev.get_object, Bucket=BUCKET, Key=key)["Body"].read()
    except Exception:
        print("no MANIFEST for", ver, flush=True)
        continue
    m = json.loads(body)
    m["dataset"] = "mv-gameplay"
    m["renamed_from"] = "cod-mv"
    rt(dev.put_object, Bucket=BUCKET, Key=key, Body=json.dumps(m, indent=2).encode())
    print("manifest updated:", ver, flush=True)

# verify parity before deleting the old prefix
new_keys = list_all(NEW)
expect = {NEW + k[len(OLD):] for k in src_keys}
missing = expect - set(new_keys)
assert not missing, f"missing {len(missing)} objects, e.g. {list(missing)[:3]}"
print("parity ok:", len(new_keys), "objects under new prefix", flush=True)

def delete_one(key):
    rt(worker_client().delete_object, Bucket=BUCKET, Key=key)

deleted = 0
with ThreadPoolExecutor(max_workers=12) as ex:
    for f in as_completed({ex.submit(delete_one, k): k for k in src_keys}):
        f.result()
        deleted += 1
print("deleted old-prefix objects:", deleted, flush=True)
print("remaining under old prefix:", len(list_all(OLD)), flush=True)
print("RENAME DONE")
