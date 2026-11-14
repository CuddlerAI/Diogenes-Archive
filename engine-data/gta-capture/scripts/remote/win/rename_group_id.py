"""rename_group_id.py <delivery dir> <src pattern> <old suffix> <new suffix>: a delivered package under a new group
name without delivering it again -- the twelve multicam_sync_i.json rewritten with the new capture_group_id (written
the way stage_multicam writes them: write_json_atomic, indent None), the other 36 files hard-linked unchanged. Each
source file is first re-serialized and checked byte-identical, so the rewrite differs in that one field only. Names come
from the pattern (Chinese does not survive the ssh command line). 2026-10-19: 113 uploaded as 2026_10_18_沙滩_001:
    python rename_group_id.py D:\\workspace\\mc4\\delivery 2026_10_18_*_113 _113 _001
Then set group.json's delivery_name to the new name, so a later full delivery comes out under it too."""
import glob, json, os, sys
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..")))   # gta-capture
from gta_capture.jsonio import write_json_atomic
out, pattern, old, new = sys.argv[1:5]
src = [p for p in glob.glob(os.path.join(out, pattern)) if os.path.isdir(p) and p.endswith(old)]
assert len(src) == 1, src
src = src[0]; name_old = os.path.basename(src); name_new = name_old[: -len(old)] + new
dst = os.path.join(out, name_new)
assert not os.path.exists(dst), dst
files = sorted(os.listdir(src)); assert len(files) == 48, len(files)
os.makedirs(dst)
for f in files:
    a, b = os.path.join(src, f), os.path.join(dst, f)
    if not f.startswith("multicam_sync_"):
        os.link(a, b); continue
    raw = open(a, "rb").read()
    doc = json.loads(raw.decode("utf-8"))
    again = (json.dumps(doc, ensure_ascii=False, indent=None, separators=(",", ":")) + "\n").encode("utf-8")
    assert again == raw, f"{f}: re-serialized bytes differ"
    assert doc["capture_group_id"] == name_old, (f, doc["capture_group_id"])
    doc["capture_group_id"] = name_new
    write_json_atomic(b, doc, indent=None)
    back = json.loads(open(b, encoding="utf-8").read())
    diff = [k for k in set(doc) | set(back) if k != "capture_group_id" and back.get(k) != json.loads(raw).get(k)]
    assert not diff and back["capture_group_id"] == name_new, (f, diff)
    print(f, "capture_group_id ->", name_new, os.path.getsize(a), "->", os.path.getsize(b), "bytes")
same = sum(1 for f in files if os.path.samefile(os.path.join(src, f), os.path.join(dst, f)))
print(json.dumps({"from": name_old, "to": name_new, "files": len(os.listdir(dst)), "hard_linked_unchanged": same,
                  "rewritten": len(files) - same}, ensure_ascii=False))
