"""合成 DataMiner converter-format (dm-2) 数据集，供 macOS 开发机全链路冒烟。

converter 的真实产出只在 Windows 采集机生成；本工具产出**同 schema** 的最小数据集
（dm1-v2 μ-law bins + `actions` 列 + 治理 meta + run_frame_ranges），让 ingest/训练/评估
在无 COD 数据的机器上也能端到端跑通。产物固定标记 non-trainable，权重即弃。

布局与 data-demo-converter ready 输出一致：
    <out>/<game>/<session>/
        metadata.json            (provenance.run_frame_ranges)
        cNNNN/{p0.mp4, p0.jsonl, meta.json}
"""
from __future__ import annotations

import argparse
import json
import math
import subprocess
from pathlib import Path

import numpy as np

from . import contract
from .action_registry import default_cod_vocab
from .physical_vocab import default_cod_physical_vocab

CHUNK_FRAMES = 120


def _write_chunk_video(path: Path, n_frames: int, w: int, h: int, fps: int,
                       seed: int) -> None:
    """用 ffmpeg testsrc 造一段可解码 CFR H.264（4s GOP）。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "lavfi", "-i", f"testsrc=size={w}x{h}:rate={fps}:decimals=2",
         "-frames:v", str(n_frames), "-c:v", "libx264", "-preset", "ultrafast",
         "-g", str(fps * 4), "-keyint_min", str(fps * 4), "-sc_threshold", "0",
         "-pix_fmt", "yuv420p", "-map_metadata", "-1", str(path)],
        check=True, capture_output=True)


def _synth_frame_row(frame: int, gframe: int, fps: int, rng: np.random.Generator,
                     actions_pool: list[str], calibrated: bool = False) -> dict:
    """造一帧标签：相关的 move/combat 语义 + 相关鼠标 delta（dm1-v2 bins）。"""
    active = [a for a in actions_pool if rng.random() < 0.25]
    # 键位：move.forward↔W 等的简化物理映射（仅供 keys aux 审计有非空信号）
    key_map = {"move.forward": 87, "move.left": 65, "move.backward": 83,
               "move.right": 68,
               "combat.reload": 82, "move.sprint": 16, "move.jump_mantle": 32}
    button_map = {"combat.primary_attack": 1, "combat.aim": 2,
                  "communication.ping": 4, "combat.melee": 5,
                  "combat.mount": 6}
    keys = sorted({key_map[a] for a in active if a in key_map})
    buttons = sorted({button_map[a] for a in active if a in button_map})
    # 偶发快速甩枪：raw Δ 超 ±200，pointerDx 会 clamp、pointerRawDx 保真（#137）
    if rng.random() < 0.1:
        raw_dx = int(rng.integers(-500, 501))
        raw_dy = int(rng.integers(-300, 301))
    else:
        raw_dx = int(rng.integers(-120, 121))
        raw_dy = int(rng.integers(-40, 41))
    dx = max(-contract.REF, min(contract.REF, raw_dx))   # pointerDx = clamp ±200
    dy = max(-contract.REF, min(contract.REF, raw_dy))
    if "camera.yaw" in actions_pool and (raw_dx or raw_dy):
        for cam in ("camera.yaw", "camera.pitch"):
            if cam not in active:
                active.append(cam)
    return {
        "frame": frame, "gframe": gframe,
        "video_t_ms": round(gframe * 1000 / fps),
        "keys": keys,
        "buttons": buttons,
        "actions": sorted(set(active)),
        "pointerDx": dx, "pointerDy": dy,
        "pointerRawDx": raw_dx, "pointerRawDy": raw_dy,
        # 视角尺度：默认未标定（与当前所有真实语料一致）；calibrated=True 时按
        # counts_per_360=18000（docs/45 的示例值）换算成弧度。
        "look_yaw_delta_rad": (raw_dx * 2 * math.pi / 18000) if calibrated else None,
        "look_pitch_delta_rad": (raw_dy * 2 * math.pi / 18000) if calibrated else None,
        "view_scale_valid": calibrated,
        "pointerBins": [contract.encode_bin(dx, "dm1-v2"),
                        contract.encode_bin(dy, "dm1-v2")],
        "events": [], "src": "rec",
    }


def synth_session(out: Path, *, game: str = "cod_steam", session: str = "synth_s0001",
                  chunks: int = 3, w: int = 256, h: int = 144, fps: int = 30,
                  n_runs: int = 2, seed: int = 0, calibrated: bool = False) -> dict:
    vocab = default_cod_vocab()
    physical_vocab = default_cod_physical_vocab()
    physical_source = physical_vocab.sources[0]
    pool = list(vocab.all_semantics)
    rng = np.random.default_rng(seed)
    session_dir = out / game / session
    total = chunks * CHUNK_FRAMES

    # 造 n_runs 个 run range（gframe 空间，frame_end 排他），一次断裂模拟暂停
    bounds = sorted(rng.choice(range(CHUNK_FRAMES, total, CHUNK_FRAMES),
                               size=n_runs - 1, replace=False)) if n_runs > 1 else []
    edges = [0, *[int(b) for b in bounds], total]
    run_ranges = [{"run_id": f"r{i:04d}", "frame_start": edges[i],
                   "frame_end": edges[i + 1]} for i in range(len(edges) - 1)]

    for c in range(chunks):
        cdir = session_dir / f"c{c:04d}"
        _write_chunk_video(cdir / "p0.mp4", CHUNK_FRAMES, w, h, fps, seed + c)
        rows = [_synth_frame_row(f, c * CHUNK_FRAMES + f, fps, rng, pool, calibrated)
                for f in range(CHUNK_FRAMES)]
        with (cdir / "p0.jsonl").open("w") as fh:
            for r in rows:
                fh.write(json.dumps(r) + "\n")
        (cdir / "meta.json").write_text(json.dumps({
            "schema_version": "dm-2",
            "catalog": None,
            "sample_key": f"{session}_c{c:04d}", "session": session, "chunk": c,
            "game": game, "category": "first_person_shooter", "platform": "pc",
            "video": {"file": "p0.mp4", "codec": "h264", "width": w, "height": h,
                      "fps": fps, "frames": CHUNK_FRAMES, "audio": False},
            "annotation_source": "raw_input_recorded",
            "key_schema": ("VK codes + dm-1 mapping "
                           f"{physical_source.mapping_id}@{physical_source.mapping_version}"),
            "rights": "local_debug_non_trainable", "first_party": False,
            "consent_id": None, "non_trainable": True, "is_trainable": False,
            "production_certified": False, "is_cutscene": False,
            "provenance": {
                "mapping_id": physical_source.mapping_id,
                "mapping_version": physical_source.mapping_version,
                "mapping_content_hash": physical_source.mapping_sha256,
            },
        }, indent=2))

    (session_dir / "metadata.json").write_text(json.dumps({
        "schema_version": "dm-2", "catalog": None,
        "workflow_id": session, "game": game,
        "provenance": {"raw_schema_version": "dm-1", "raw_session_id": session,
                       "mapping_id": physical_source.mapping_id,
                       "mapping_version": physical_source.mapping_version,
                       "mapping_content_hash": physical_source.mapping_sha256,
                       "run_frame_ranges": run_ranges},
        "frame_pairs": {"gap_h": 8, "stride": 4, "overlapping": True},
    }, indent=2))
    return {"session_dir": str(session_dir), "chunks": chunks,
            "frames": total, "run_ranges": run_ranges}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--chunks", type=int, default=3)
    ap.add_argument("--runs", type=int, default=2)
    ap.add_argument("--width", type=int, default=256)
    ap.add_argument("--height", type=int, default=144)
    ap.add_argument("--seed", type=int, default=0)
    args = ap.parse_args()
    print(json.dumps(synth_session(args.out, chunks=args.chunks, w=args.width,
                                   h=args.height, n_runs=args.runs, seed=args.seed),
                     indent=2))


if __name__ == "__main__":
    main()
