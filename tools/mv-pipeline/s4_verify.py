"""S4: ml-side compliance-gate and readability verification against the
uploaded method-validation dataset. Downloads one shard, extracts chunks,
runs the real contract gate both ways, decodes one chunk's mp4."""
import boto3, os, json, sys, tarfile, io, hashlib
from botocore.config import Config

sys.path.insert(0, "/Users/example/Documents/Diogenes/.claude/worktrees/modest-taussig-cdac36/ml")
from diogenes_phase0.contract import ChunkMeta, ComplianceError, gate_chunks, load_actions

SCRATCH = "/private/tmp/claude-501/-Users-oratis-Documents-Diogenes--claude-worktrees-modest-taussig-cdac36/a1d262fc-eaf0-429f-9d25-10d579aed992/scratchpad"
BASE = "method-validation/cod-mv/v2026.07"
BUCKET = "diogenes-training-dev"
cfg = Config(signature_version="s3v4", region_name="auto", retries={"max_attempts": 8})
dev = boto3.client("s3", endpoint_url=os.environ["RCLONE_CONFIG_R2DEV_ENDPOINT"],
    aws_access_key_id=os.environ["RCLONE_CONFIG_R2DEV_ACCESS_KEY_ID"],
    aws_secret_access_key=os.environ["RCLONE_CONFIG_R2DEV_SECRET_ACCESS_KEY"], config=cfg)

manifest = json.loads(dev.get_object(Bucket=BUCKET, Key=f"{BASE}/MANIFEST.json")["Body"].read())
shards = json.loads(dev.get_object(Bucket=BUCKET, Key=f"{BASE}/index/shards.json")["Body"].read())["shards"]
# smallest shard to keep the download cheap
target = min(shards, key=lambda s: s["bytes"])
print("downloading shard:", target["key"], target["bytes"], "bytes")
body = dev.get_object(Bucket=BUCKET, Key=f"{BASE}/{target['key']}")["Body"].read()
assert hashlib.sha256(body).hexdigest() == target["sha256"], "shard sha256 mismatch vs index"
print("PASS shard sha256 matches shards.json")

tf = tarfile.open(fileobj=io.BytesIO(body))
names = tf.getnames()
metas_raw = [n for n in names if n.endswith(".meta.json")]
print("tar entries:", len(names), "chunks:", len(metas_raw))

extract_dir = f"{SCRATCH}/s4-chunks"
os.makedirs(extract_dir, exist_ok=True)
chunk_metas = []
first_chunk_prefix = metas_raw[0][: -len(".meta.json")]
for n in names:
    if n.startswith(first_chunk_prefix):
        with open(os.path.join(extract_dir, n.split("/")[-1].replace(first_chunk_prefix + ".", "")), "wb") as f:
            f.write(tf.extractfile(n).read())
for n in metas_raw:
    chunk_metas.append(ChunkMeta.from_json(json.loads(tf.extractfile(n).read())))

m = chunk_metas[0]
print("chunk meta:", {"rights": m.rights, "first_party": m.first_party, "consent_id": m.consent_id,
                       "is_trainable": m.is_trainable, "tags": m.tags})

# 1) negative test: default gate MUST refuse
try:
    gate_chunks(chunk_metas, allow_method_validation_only=False)
    print("FAIL: gate did NOT block method-validation chunks"); sys.exit(1)
except ComplianceError as e:
    print("PASS gate blocks without flag:", str(e)[:120])

# 2) positive: explicit method-validation flag admits
gate_chunks(chunk_metas, allow_method_validation_only=True)
print("PASS gate admits with explicit method-validation flag")

# 3) is_trainable gate semantics: every chunk must be non-trainable
from diogenes_phase0.contract import is_trainable_meta
assert not any(is_trainable_meta(m) for m in chunk_metas), "a chunk claims trainable!"
print("PASS all", len(chunk_metas), "chunks are non-trainable per gate semantics")

# 4) readability: p0.jsonl parses as action frames; mp4 decodes; counts agree
actions = load_actions(os.path.join(extract_dir, "p0.jsonl"))
import cv2
cap = cv2.VideoCapture(os.path.join(extract_dir, "p0.mp4"))
frames = 0
while True:
    ok, _ = cap.read()
    if not ok: break
    frames += 1
cap.release()
print("action rows:", len(actions), "decoded mp4 frames:", frames)
assert len(actions) == frames, "frame/action count mismatch"
print("PASS chunk readable: frames == action rows ==", frames)
print("S4 ALL PASS")
