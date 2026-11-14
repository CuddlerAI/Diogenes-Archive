"""compare_packages.py <old package dir> <new package dir>: what changed between two deliveries of a group -- the
files whose bytes differ, and in each changed JSON what differs: action_camera_i.json the per-frame fields and how
many frames each differs in (the camera's pitch range, yaw and position change at most, mouse frames); multicam_sync
the header keys and the frames changed besides each listed key.

Every redelivery is set aside first (the old package renamed .superseded-<why>) and compared this way: only the files
the change was meant to touch may differ (beach-demo-113, 2026-10-18: holding the first person level changed exactly
video_8/12 and action_camera_8/12; the sun filled in changed exactly the twelve multicam_sync files, and in them only
each frame's lighting and the capture note)."""
import hashlib
import json
import math
import os
import sys


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def action_camera(a, b):
    fields = {}
    for ra, rb in zip(a, b):
        for k in set(ra) | set(rb):
            if ra.get(k) != rb.get(k):
                fields[k] = fields.get(k, 0) + 1
    out = [f"{len(a)}/{len(b)} records; fields differing (frames): {dict(sorted(fields.items()))}"]
    if "camera_rotation_oula" in fields:
        pa = [r["camera_rotation_oula"][0] for r in a]
        pb = [r["camera_rotation_oula"][0] for r in b]
        yaw = max(abs((ra["camera_rotation_oula"][1] - rb["camera_rotation_oula"][1] + 180) % 360 - 180) for ra, rb in zip(a, b))
        out.append(f"pitch {min(pa):.2f}..{max(pa):.2f} -> {min(pb):.2f}..{max(pb):.2f}; yaw differs at most {yaw:.4f} deg")
    if "camera_position" in fields:
        move = max(math.dist(ra["camera_position"], rb["camera_position"]) for ra, rb in zip(a, b))
        out.append(f"camera moved at most {move * 100:.1f} cm")
    for k in ("mouse_dx", "mouse_dy"):
        if k in fields:
            out.append(f"{k} non-zero frames {sum(1 for r in a if r.get(k))} -> {sum(1 for r in b if r.get(k))}")
    return out


def multicam(a, b):
    head = sorted(k for k in set(a) | set(b) if k != "frames" and a.get(k) != b.get(k))
    keys = sorted({k for f in a["frames"] + b["frames"] for k in f})
    per_key = {k: sum(1 for fa, fb in zip(a["frames"], b["frames"]) if fa.get(k) != fb.get(k)) for k in keys}
    return [f"header keys changed {head}; frames differing per key {per_key}"]


def main(argv) -> int:
    old, new = argv
    names = sorted(set(os.listdir(old)) | set(os.listdir(new)))
    changed = []
    for name in names:
        a, b = os.path.join(old, name), os.path.join(new, name)
        if not (os.path.isfile(a) and os.path.isfile(b)):
            print("ONLY IN", "old" if os.path.isfile(a) else "new", name)
            continue
        if os.path.getsize(a) != os.path.getsize(b) or sha(a) != sha(b):
            changed.append(name)
    print(f"{len(names)} files, {len(changed)} changed: {changed}")
    for name in changed:
        if name.startswith("action_camera_"):
            lines = action_camera(load(os.path.join(old, name)), load(os.path.join(new, name)))
        elif name.startswith("multicam_sync_"):
            lines = multicam(load(os.path.join(old, name)), load(os.path.join(new, name)))
        else:
            continue
        print(name)
        for line in lines:
            print("  " + line)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
