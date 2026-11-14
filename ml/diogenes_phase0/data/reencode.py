"""Re-encode contract chunks from npz frames to video.

    python -m diogenes_phase0.data.reencode --src data/idm_minecraft \
        --dst data/idm_minecraft_mp4 [--codec mp4] [--limit 200]

Two uses. The first is the confound check that the mp4 switch owes the npz
runs: evaluating one checkpoint on a lossless split and on a re-encode of
that same split isolates what the codec costs, with no second training run
and no change to the data the comparison rests on. The second is plain
archival — the npz form of one game's 4.6 usable hours is 71 GB.

Labels are copied byte-for-byte rather than regenerated, so the only thing
that differs between src and dst is the pixels.
"""
from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

import numpy as np

from .dataset import scan_dataset
from .gaming500 import _codec_name, _write_frames


def reencode_chunk(chunk_dir: Path, out_dir: Path, codec: str = "mp4",
                   fps: float = 30.0) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    meta = json.loads((chunk_dir / "meta.json").read_text())
    shutil.copy2(chunk_dir / "p0.jsonl", out_dir / "p0.jsonl")

    src_npz = chunk_dir / "p0.npz"
    if src_npz.exists():
        frames = np.load(src_npz)["frames"]
        _write_frames(out_dir, frames, codec, fps)
        video = dict(meta.get("video") or {})
        video["file"] = "p0.npz" if codec == "npz" else "p0.mp4"
        video["codec"] = _codec_name(codec)
        meta["video"] = video
    elif (chunk_dir / "p0.mp4").exists():
        shutil.copy2(chunk_dir / "p0.mp4", out_dir / "p0.mp4")

    (out_dir / "meta.json").write_text(json.dumps(meta, indent=2))


def reencode_dataset(src: Path, dst: Path, *, codec: str = "mp4",
                     limit: int | None = None) -> dict:
    records = scan_dataset(src)
    if not records:
        raise SystemExit(f"no contract-format chunks under {src}")
    if limit:
        records = records[:limit]
    src_bytes = dst_bytes = 0
    for rec in records:
        rel = rec.chunk_dir.relative_to(src)
        out_dir = dst / rel
        reencode_chunk(rec.chunk_dir, out_dir, codec=codec)
        src_bytes += sum(f.stat().st_size for f in rec.chunk_dir.iterdir())
        dst_bytes += sum(f.stat().st_size for f in out_dir.iterdir())
    # session-level sidecars the loader does not read but the provenance does
    for side in src.rglob("metadata.json"):
        tgt = dst / side.relative_to(src)
        if tgt.parent.exists() and not tgt.exists():
            shutil.copy2(side, tgt)
    return {"chunks": len(records), "src_bytes": src_bytes,
            "dst_bytes": dst_bytes,
            "ratio": round(src_bytes / max(1, dst_bytes), 2)}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True, type=Path)
    ap.add_argument("--dst", required=True, type=Path)
    ap.add_argument("--codec", choices=["npz", "mp4"], default="mp4")
    ap.add_argument("--limit", type=int, default=None,
                    help="re-encode only the first N chunks (codec probe)")
    args = ap.parse_args()
    stats = reencode_dataset(args.src, args.dst, codec=args.codec,
                             limit=args.limit)
    print(json.dumps(stats, indent=2))


if __name__ == "__main__":
    main()
