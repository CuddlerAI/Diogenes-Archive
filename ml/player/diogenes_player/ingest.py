"""dm-1 DATASET 平面（converter DataModel 产物）→ 训练代理平面。

输入: **深度无关地**发现所有 `cNNNN/meta.json`；chunk 的父目录即 session dir。
      支持真实 converter 布局 `ready/<session>/<game_slug>/<session>/cNNNN`、
      data_demo 布局 `<game>/<session>/cNNNN`、synth 布局，无需 --src 指到特定层级。
      game/session 身份从 **meta.json 字段**读取（不靠路径位置），plane 目录用其 slug。
输出: <out>/<game_slug>/<session>/{cNNNN.mp4, actions.parquet}
      <out>/index.parquet      chunk 级索引（治理列 + gframe_start + mu_law_scheme）
      <out>/run_ranges.json    session → run_frame_ranges（采样不跨 run 的依据）
      <out>/_ingest_stats.json （含 trainable/non_trainable 计数，供操作员看治理门状态）

设计要点 (research/07 §5):
- 训练平面是可丢弃派生缓存；consent 撤销后随源重物化。
- 全程对账: JSONL 行数 == meta.video.frames == 代理视频帧数。
- bins 从 raw dx/dy 用指定 scheme 重算（消除两代刻度混入）；--strict-bins 时额外
  断言重算==存储（真实 dm1-v2 数据应恒等，捕获 DataMiner 公式再漂移）。
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import time
from collections import Counter
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from . import contract
from .physical_vocab import (
    PhysicalInputContractError,
    PhysicalInputVocab,
    add_physical_vocab_arguments,
    physical_vocab_from_args,
    validate_physical_vocab_scheme,
)
from .video import count_frames

_CHUNK_RE = re.compile(r"^c\d+$")


def _slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-") or "unknown"


def discover_sessions(src: Path) -> list[Path]:
    """深度无关：返回所有含 `cNNNN/meta.json` 的 session 目录（chunk 的父目录），去重排序。"""
    session_dirs = {
        m.parent.parent for m in src.rglob("meta.json")
        if _CHUNK_RE.match(m.parent.name)
    }
    return sorted(session_dirs)

ACTION_SCHEMA = pa.schema([
    ("game", pa.string()),
    ("session", pa.string()),
    ("chunk", pa.int32()),
    ("frame", pa.int32()),
    ("gframe", pa.int64()),
    ("physical_inputs_mask", pa.uint64()),
    ("actions", pa.list_(pa.string())),   # dm-1 语义动作（BC 主目标）
    ("bin_x", pa.int8()),
    ("bin_y", pa.int8()),
    ("dx", pa.int16()),          # pointerDx（clamp ±200）
    ("dy", pa.int16()),
    ("raw_dx", pa.int32()),      # pointerRawDx（未截断真实 Δ，#137）；甩枪可超 int16
    ("raw_dy", pa.int32()),
    # 视角尺度标定（docs/45 §123 训练派生层）：弧度制、与灵敏度和游戏无关。
    # 未标定时为 null——训练侧必须据此拒绝把 camera 头训成灵敏度锁死的。
    ("look_yaw_delta_rad", pa.float32()),
    ("look_pitch_delta_rad", pa.float32()),
    ("view_scale_valid", pa.bool_()),
    ("src", pa.string()),
])

INDEX_SCHEMA = pa.schema([
    ("game", pa.string()),
    ("session", pa.string()),
    ("chunk", pa.int32()),
    ("video", pa.string()),               # 相对 out 根
    ("actions_parquet", pa.string()),     # session actions.parquet 相对路径
    ("row_offset", pa.int64()),           # 该 chunk 在 session actions.parquet 的起始行
    ("n_frames", pa.int32()),
    ("gframe_start", pa.int64()),         # 该 chunk 首帧的会话内 gframe
    ("fps", pa.float32()),
    ("width", pa.int32()),
    ("height", pa.int32()),
    ("is_trainable", pa.bool_()),
    ("rights", pa.string()),
    ("annotation_source", pa.string()),
    ("mu_law_scheme", pa.string()),       # bins 用的 μ-law 公式（可复现凭证）
    ("physical_vocab_hash", pa.string()), # physical_input_vocab.json canonical hash
    ("view_scale_frames", pa.int32()),    # 该 chunk 内 view_scale_valid 的帧数
])


def transcode_proxy(src: Path, dst: Path, *, width: int, height: int,
                    gop: int, crf: int) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
        "-i", str(src),
        "-vf", f"scale={width}:{height}:flags=bicubic",
        "-c:v", "libx264", "-preset", "veryfast", "-crf", str(crf),
        "-g", str(gop), "-keyint_min", str(gop), "-sc_threshold", "0",
        "-pix_fmt", "yuv420p", "-an", "-movflags", "+faststart",
        "-map_metadata", "-1",
        str(dst),
    ]
    subprocess.run(cmd, check=True, capture_output=True)


def ingest_chunk(chunk_dir: Path, out_root: Path, game: str, session: str,
                  plane_rel: Path, *, width: int, height: int, gop: int, crf: int,
                  verify_frames: bool, scheme: str,
                  strict_bins: bool,
                  physical_vocab: PhysicalInputVocab) -> tuple[list[dict], dict]:
    meta = json.loads((chunk_dir / "meta.json").read_text(encoding="utf-8"))
    physical_source = physical_vocab.validate_chunk_meta(
        meta, context=str(chunk_dir / "meta.json"))
    chunk_id = int(meta["chunk"])
    fps = float(meta["video"]["fps"])
    n_expected = int(meta["video"]["frames"])

    rows: list[dict] = []
    gframe_start: int | None = None
    with (chunk_dir / "p0.jsonl").open(encoding="utf-8") as f:
        for line in f:
            # 从 raw dx/dy 用统一 scheme 重算 bins（丢弃存储 bins，消除两代刻度混入,
            # research/07 §3）。strict_bins 时额外断言重算==存储（真实 dm1-v2 恒等）。
            raw = json.loads(line)
            fa = contract.parse_frame_row(raw, verify_bins=strict_bins,
                                          scheme=scheme)
            physical_mask = physical_vocab.encode_mask(
                raw.get("keys", []), raw.get("buttons", []),
                source=physical_source,
                context=f"{chunk_dir / 'p0.jsonl'} frame={raw.get('frame')}",
            )
            if gframe_start is None:
                gframe_start = fa.gframe
            rows.append({
                "game": game,
                "session": session,
                "chunk": chunk_id,
                "frame": fa.frame,
                "gframe": fa.gframe,
                "physical_inputs_mask": physical_mask,
                "actions": list(fa.actions),
                "bin_x": contract.encode_bin(fa.dx, scheme),
                "bin_y": contract.encode_bin(fa.dy, scheme),
                "dx": fa.dx,
                "dy": fa.dy,
                "raw_dx": fa.raw_dx,
                "raw_dy": fa.raw_dy,
                "look_yaw_delta_rad": fa.look_yaw_delta_rad,
                "look_pitch_delta_rad": fa.look_pitch_delta_rad,
                "view_scale_valid": fa.view_scale_valid,
                "src": fa.src,
            })
    if len(rows) != n_expected:
        raise ValueError(f"{chunk_dir}: jsonl rows {len(rows)} != meta frames {n_expected}")
    view_scale_frames = sum(1 for r in rows if r["view_scale_valid"])

    proxy_rel = plane_rel / f"c{chunk_id:04d}.mp4"
    proxy_path = out_root / proxy_rel
    transcode_proxy(chunk_dir / "p0.mp4", proxy_path, width=width, height=height,
                    gop=gop, crf=crf)
    if verify_frames:
        got = count_frames(str(proxy_path))
        if got != n_expected:
            raise ValueError(f"{proxy_path}: proxy frames {got} != {n_expected}")

    index_row = {
        "game": game,
        "session": session,
        "chunk": chunk_id,
        "video": str(proxy_rel),
        "actions_parquet": str(plane_rel / "actions.parquet"),
        "row_offset": -1,  # session 汇总时回填
        "n_frames": n_expected,
        "gframe_start": int(gframe_start or 0),
        "fps": fps,
        "width": width,
        "height": height,
        "is_trainable": bool(meta.get("is_trainable", False)),
        "rights": str(meta.get("rights", "unknown")),
        "annotation_source": str(meta.get("annotation_source", "unknown")),
        "mu_law_scheme": scheme,
        "physical_vocab_hash": physical_vocab.content_hash,
        "view_scale_frames": view_scale_frames,
    }
    return rows, {"index": index_row}


def _load_run_ranges(session_dir: Path) -> list[dict]:
    """从 converter session metadata.json 读 run_frame_ranges（gframe 空间, frame_end 排他）。
    缺失（如 data_demo 存量）时返回空 → dataset 视整个 session 为单一 range。"""
    meta_f = session_dir / "metadata.json"
    if not meta_f.exists():
        return []
    try:
        prov = json.loads(meta_f.read_text(encoding="utf-8")).get("provenance", {})
        return [{"run_id": r["run_id"], "frame_start": int(r["frame_start"]),
                 "frame_end": int(r["frame_end"])}
                for r in prov.get("run_frame_ranges", [])]
    except (json.JSONDecodeError, KeyError, TypeError):
        return []


def _session_identity(session_dir: Path) -> tuple[str, str]:
    """从任一 chunk meta.json 读 (game, session)。身份来自字段, 不靠路径位置。"""
    first_meta = next(
        (m for m in sorted(session_dir.glob("c*/meta.json"))), None)
    if first_meta is None:
        raise ValueError(f"{session_dir}: 无 chunk meta.json")
    meta = json.loads(first_meta.read_text(encoding="utf-8"))
    return str(meta.get("game", "unknown")), str(meta["session"])


def audit_physical_inputs(src: Path, physical_vocab: PhysicalInputVocab) -> dict:
    """Read every source label before any proxy is written.

    The report is deliberately count-based: it is useful for diagnosing a
    catalog drift, but ingest still fails closed whenever either counter is
    non-zero or a mapping identity does not match the pinned sources.
    """
    unknown_keys: Counter[int] = Counter()
    unknown_buttons: Counter[int] = Counter()
    identity_errors: list[str] = []
    frames = 0
    chunks = 0
    for session_dir in discover_sessions(src):
        for chunk_dir in sorted(d for d in session_dir.glob("c*")
                                if _CHUNK_RE.match(d.name)):
            meta_path = chunk_dir / "meta.json"
            actions_path = chunk_dir / "p0.jsonl"
            if not meta_path.exists() or not actions_path.exists():
                identity_errors.append(f"{chunk_dir}: missing meta.json or p0.jsonl")
                continue
            chunks += 1
            physical_source = None
            try:
                physical_source = physical_vocab.validate_chunk_meta(
                    json.loads(meta_path.read_text(encoding="utf-8")),
                    context=str(meta_path),
                )
            except (json.JSONDecodeError, PhysicalInputContractError) as exc:
                identity_errors.append(str(exc))
            allowed_keys = set(
                physical_source.keyboard_vks
                if physical_source is not None
                else physical_vocab.keyboard_vks
            )
            allowed_buttons = set(
                physical_source.mouse_buttons
                if physical_source is not None
                else physical_vocab.mouse_buttons
            )
            with actions_path.open(encoding="utf-8") as handle:
                for line in handle:
                    row = json.loads(line)
                    frames += 1
                    unknown_keys.update(
                        int(v) for v in row.get("keys", []) if int(v) not in allowed_keys)
                    unknown_buttons.update(
                        int(v) for v in row.get("buttons", [])
                        if int(v) not in allowed_buttons)
    return {
        "physical_vocab_hash": physical_vocab.content_hash,
        "chunks": chunks,
        "frames": frames,
        "unknown_keyboard_vks": dict(sorted(unknown_keys.items())),
        "unknown_mouse_buttons": dict(sorted(unknown_buttons.items())),
        "identity_errors": identity_errors,
        "blocking": bool(unknown_keys or unknown_buttons or identity_errors),
    }


def ingest(src: Path, out: Path, *, physical_vocab: PhysicalInputVocab,
           width: int = 256, height: int = 144,
           gop: int = 30, crf: int = 23, limit_chunks: int | None = None,
           verify_frames: bool = True, scheme: str = contract.DEFAULT_SCHEME,
           strict_bins: bool = False) -> dict:
    t0 = time.time()
    validate_physical_vocab_scheme(physical_vocab, scheme)
    physical_audit = audit_physical_inputs(src, physical_vocab)
    if physical_audit["blocking"]:
        raise PhysicalInputContractError(
            "physical input pre-audit failed: "
            + json.dumps(physical_audit, ensure_ascii=False, sort_keys=True)
        )
    out.mkdir(parents=True, exist_ok=True)
    physical_vocab.write(out / "physical_input_vocab.json")
    index_rows: list[dict] = []
    run_ranges: dict[str, list[dict]] = {}
    stats = {"chunks": 0, "frames": 0, "sessions": 0,
             "scheme": scheme, "trainable_chunks": 0, "non_trainable_chunks": 0,
              "view_scale_frames": 0,
              "physical_vocab_hash": physical_vocab.content_hash,
              "physical_input_audit": physical_audit,
              "games": {}, "failed": []}

    for session_dir in discover_sessions(src):
        try:
            game, session = _session_identity(session_dir)
        except Exception as e:  # noqa: BLE001
            stats["failed"].append({"session": str(session_dir), "error": str(e)})
            continue
        plane_rel = Path(_slug(game)) / _slug(session)
        chunk_dirs = sorted(d for d in session_dir.glob("c*")
                            if _CHUNK_RE.match(d.name) and (d / "meta.json").exists())
        if limit_chunks:
            chunk_dirs = chunk_dirs[:limit_chunks]
        session_rows: list[dict] = []
        for chunk_dir in chunk_dirs:
            try:
                rows, info = ingest_chunk(
                    chunk_dir, out, game, session, plane_rel,
                    width=width, height=height, gop=gop, crf=crf,
                    verify_frames=verify_frames, scheme=scheme,
                    strict_bins=strict_bins, physical_vocab=physical_vocab)
            except Exception as e:  # noqa: BLE001 — 单 chunk 失败不拖垮全量, 但必须留痕
                stats["failed"].append({"chunk": str(chunk_dir), "error": str(e)})
                continue
            info["index"]["row_offset"] = len(session_rows)
            session_rows.extend(rows)
            index_rows.append(info["index"])
            stats["chunks"] += 1
            stats["frames"] += len(rows)
            stats["view_scale_frames"] += info["index"]["view_scale_frames"]
            if info["index"]["is_trainable"]:
                stats["trainable_chunks"] += 1
            else:
                stats["non_trainable_chunks"] += 1
        if session_rows:
            table = pa.Table.from_pylist(session_rows, schema=ACTION_SCHEMA)
            dst = out / plane_rel / "actions.parquet"
            dst.parent.mkdir(parents=True, exist_ok=True)
            pq.write_table(table, dst)
            run_ranges[f"{_slug(game)}/{_slug(session)}"] = _load_run_ranges(session_dir)
            stats["games"][game] = stats["games"].get(game, 0) + 1
            stats["sessions"] += 1

    if index_rows:
        pq.write_table(pa.Table.from_pylist(index_rows, schema=INDEX_SCHEMA),
                       out / "index.parquet")
    (out / "run_ranges.json").write_text(
        json.dumps(run_ranges, indent=2), encoding="utf-8")
    frames = stats["frames"]
    stats["view_scale_coverage_pct"] = (
        round(100 * stats["view_scale_frames"] / frames, 2) if frames else 0.0)
    stats["elapsed_s"] = round(time.time() - t0, 2)
    (out / "_ingest_stats.json").write_text(
        json.dumps(stats, indent=2), encoding="utf-8")
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", type=Path, required=True, help="dm-1 dataset 根目录")
    ap.add_argument("--out", type=Path, required=True, help="训练平面输出目录")
    ap.add_argument("--width", type=int, default=256)
    ap.add_argument("--height", type=int, default=144)
    ap.add_argument("--gop", type=int, default=30, help="代理视频 GOP 帧数 (默认 1s@30fps)")
    ap.add_argument("--crf", type=int, default=23)
    ap.add_argument("--limit-chunks", type=int, default=None, help="每 session 只取前 N 个 chunk")
    ap.add_argument("--no-verify-frames", action="store_true")
    ap.add_argument("--scheme", default=contract.DEFAULT_SCHEME,
                    choices=["dm1-v2", "legacy"],
                    help="μ-law 分箱公式：新式 DataMiner 产物用 dm1-v2；data_demo 存量用 legacy")
    ap.add_argument("--strict-bins", action="store_true",
                    help="断言重算 bins == 存储 bins（真实 dm1-v2 数据应恒等；捕获契约漂移）")
    add_physical_vocab_arguments(ap)
    args = ap.parse_args()
    physical_vocab = physical_vocab_from_args(args)
    stats = ingest(args.src, args.out, physical_vocab=physical_vocab,
                   width=args.width, height=args.height,
                   gop=args.gop, crf=args.crf, limit_chunks=args.limit_chunks,
                   verify_frames=not args.no_verify_frames, scheme=args.scheme,
                   strict_bins=args.strict_bins)
    print(json.dumps(stats, indent=2))


if __name__ == "__main__":
    main()
