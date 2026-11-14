#!/usr/bin/env python3
"""Stitch consecutive capture segments back into one continuous clip directory.

The client writes a recording as gapless 10-second segments (crash containment:
a force-kill costs the in-flight segment, not the take). That is a storage
shape, not the shape of the material — 93.9% of the corpus sits in runs of 15
minutes or more, and `session_frame` runs unbroken across the segment boundary.

Demos that show isolated 10-second clips therefore undersell the data: they
read as "we have fragments" when what we have is long continuous play stored in
fragments. This tool reverses the split so a walkthrough can show what the run
actually was.

Output is a directory that `gameplay_render.py` accepts unchanged:

    video.mp4  frames.jsonl  events.jsonl  audio.pcm  meta.json

Contiguity is enforced, not assumed. The tool refuses to join segments whose
`session_frame` does not continue exactly where the previous one stopped —
silently splicing across a gap would put a cut in the middle of a clip we are
presenting as continuous, which is the one thing this tool exists to prevent.

Usage:
    concat_segments.py --segments DIR [DIR ...] --out DIR
    concat_segments.py --manifest list.txt --out DIR     # one dir per line
"""
import argparse, glob, json, os, shutil, subprocess, sys

import zstandard


def find(d, *stems):
    """Delivered files carry a -<sha256> suffix; accept both shapes."""
    for stem in stems:
        base, ext = stem.split(".", 1)
        for pat in (os.path.join(d, stem), os.path.join(d, f"{base}-*.{ext}")):
            hit = sorted(glob.glob(pat))
            if hit:
                return hit[0]
    return None


def read_raw(path):
    raw = open(path, "rb").read()
    if path.endswith(".zst"):
        raw = zstandard.ZstdDecompressor().decompress(raw, max_output_size=1 << 31)
    return raw


def read_rows(path):
    if path is None:
        return []
    return [json.loads(l) for l in read_raw(path).decode("utf-8").splitlines() if l.strip()]


def load(d):
    mp = find(d, "meta.json")
    if not mp:
        sys.exit(f"{d}: no meta.json")
    m = json.load(open(mp))
    return dict(dir=d, meta=m, seg=m.get("segment_index"),
                frames=read_rows(find(d, "frames.jsonl.zst", "frames.jsonl")),
                events=read_rows(find(d, "events.jsonl.zst", "events.jsonl")),
                video=find(d, "video.mp4"),
                audio=find(d, "audio.pcm.zst", "audio.pcm"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--segments", nargs="*", default=[])
    ap.add_argument("--manifest")
    ap.add_argument("--out", required=True)
    ap.add_argument("--allow-gaps", action="store_true",
                    help="join anyway across a session_frame discontinuity (prints the gap)")
    a = ap.parse_args()

    dirs = list(a.segments)
    if a.manifest:
        dirs += [l.strip() for l in open(a.manifest) if l.strip()]
    if len(dirs) < 2:
        sys.exit("need at least two segment directories")

    segs = sorted((load(d) for d in dirs), key=lambda s: (s["seg"] is None, s["seg"]))
    os.makedirs(a.out, exist_ok=True)

    # --- contiguity gate ------------------------------------------------
    prev = None
    for s in segs:
        first = s["frames"][0].get("session_frame") if s["frames"] else None
        if prev is not None and first is not None and first != prev:
            msg = (f"gap before segment {s['seg']}: session_frame jumps "
                   f"{prev} -> {first} ({first - prev} frames)")
            if not a.allow_gaps:
                sys.exit("refusing to splice — " + msg +
                         "\n(pass --allow-gaps only if a visible cut is acceptable)")
            print("WARNING " + msg, file=sys.stderr)
        if s["frames"]:
            last = s["frames"][-1].get("session_frame")
            prev = last + 1 if last is not None else None

    print(f"segments {[s['seg'] for s in segs]}  "
          f"frames {sum(len(s['frames']) for s in segs)}", flush=True)

    # --- video: stream copy, no re-encode -------------------------------
    lst = os.path.join(a.out, "_concat.txt")
    with open(lst, "w") as f:
        for s in segs:
            if not s["video"]:
                sys.exit(f"{s['dir']}: no video.mp4")
            f.write(f"file '{os.path.abspath(s['video'])}'\n")
    out_video = os.path.join(a.out, "video.mp4")
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0",
                    "-i", lst, "-c", "copy", "-movflags", "+faststart", out_video],
                   check=True)
    os.remove(lst)

    # --- frames / events: order preserved, rows untouched ---------------
    # The renderer indexes frames positionally, and the rows are what a buyer
    # receives — rewriting `frame` to be run-relative would make the demo show
    # something the delivered file does not say. Left verbatim.
    for name, key in (("frames.jsonl", "frames"), ("events.jsonl", "events")):
        with open(os.path.join(a.out, name), "w") as f:
            for s in segs:
                for r in s[key]:
                    f.write(json.dumps(r, separators=(",", ":")) + "\n")

    # --- audio: raw s16le, concatenation is append ----------------------
    if all(s["audio"] for s in segs):
        with open(os.path.join(a.out, "audio.pcm"), "wb") as f:
            for s in segs:
                f.write(read_raw(s["audio"]))

    # --- meta: first segment's provenance, spans widened ----------------
    m = dict(segs[0]["meta"])
    nf = sum(len(s["frames"]) for s in segs)
    m["segment_index"] = segs[0]["seg"]
    m["concatenated_from"] = [s["seg"] for s in segs]
    m["n_frames"] = nf
    if segs[-1]["meta"].get("qpc_end") is not None:
        m["qpc_end"] = segs[-1]["meta"]["qpc_end"]
    for k, n in (("frames", nf), ("events", sum(len(s["events"]) for s in segs))):
        if isinstance(m.get(k), dict):
            m[k] = dict(m[k], lines=n)
            m[k].pop("sha256", None)      # no longer the delivered file's hash
    if isinstance(m.get("video"), dict):
        m["video"] = dict(m["video"], bytes=os.path.getsize(out_video))
        m["video"].pop("sha256", None)
    if isinstance(m.get("audio"), dict):
        m["audio"] = dict(m["audio"])
        m["audio"].pop("sha256", None)
        ap_ = os.path.join(a.out, "audio.pcm")
        if os.path.exists(ap_):
            m["audio"]["bytes"] = os.path.getsize(ap_)
    json.dump(m, open(os.path.join(a.out, "meta.json"), "w"), indent=1)

    secs = nf / (m.get("fps_num", 60) / m.get("fps_den", 1))
    print(f"wrote {a.out}: {nf} frames = {secs:.1f} s continuous")


if __name__ == "__main__":
    main()
