"""Lane A · COD 评估样本 (parquet + mp4) → chunk 契约目录, 供零训练体检消费.

    python -m diogenes_wm.cod_adapter --src <DiogenesData/full> --out <dir> \
        --max-segments-per-session 4

## 两条车道, 由 `--rights` 选, 默认是不可训练的那条

**默认(`--rights third_party`)只服务于不训练的体检**(research/33 Lane A: 核验录制
质量 / 验证标注 schema / 检查帧-事件对齐)。它把源数据的合规戳**逐字**抄下来:

    rights=local_debug_non_trainable · first_party=false · consent_id=null
    non_trainable=true · is_trainable=false · production_certified=false

于是 `contract.is_trainable_meta()` 返回 False, `gate_chunks()` 默认硬拦截。

**`--rights first_party --consent-id <owner 具结 ID>` 是 owner 具结车道**
(2026-08-01 裁决, research/33 §1.3)。owner 的认定是: 这批录制是**自采**数据,
本轮训练是**方法验证**, 产物**不会在任何情况下发布和使用**。该车道戳:

    rights=first_party · first_party=true · consent_id=<具结 ID>
    non_trainable=false · is_trainable=true · production_certified=**false**

三点刻意的设计:

1. **必须同时给 `--consent-id`**, 缺了就报错。`is_trainable` 单独翻不过门 ——
   门是四项与, 单翻一个只会让人以为改了其实没改。把三个字段绑在一个显式开关后面,
   是为了让"翻车道"永远是一次有意的动作, 不会因为改了一个字段而半生效。
2. **`production_certified` 恒为 false**, 两条车道都是。它是产品认证位, 而 owner
   的具结说的是"验证用、不发布" —— 那**不是**产品认证, 两者不能混为一谈。
   转换器本身也拒收任何 RAW 侧的 certified 声明(`lib.rs` 的两处 ensure)。
3. **`no_publish` / `owner_attested_validation` 标签跟着数据走**, 写进每个
   `meta.json` 的 `tags`。"不会发布和使用"这条约束必须留在数据上, 而不是只留在
   某一次对话里 —— 三个月后翻到这批 chunk 的人只看得到 `meta.json`。

⚠ 视频内容是第三方 AAA(Call of Duty®, 版权归 Activision)。owner 具结覆盖的是
**采集与内部验证**, 不是内容授权 —— 所以 `no_publish` 不是形式主义, 它是这条车道
成立的前提。产物不得进产品语料、不得对外分发、不得进任何发布的 checkpoint。

## 为什么从 COD 侧接, 不从 MV 侧接

两份数据同源(MV 的 18 个 session 是 COD 19 个的真子集), 但 COD 侧多一张
`events.parquet` —— 原始输入事件流。有了它, **每帧的未截断指针位移可以直接
重算**: 按帧的 `[output_qpc_start, output_qpc_end)` 半开区间求和该区间内所有
`mouse_move` 的 dx/dy。这正是 `pointerRawDx/Dy` 的定义, 而 MV 的 v2026.07
分片里没有这一对(v2026.08 才补上时间坐标)。

区别是承重的: `pointer_dx/dy` 已被转换器 clamp 到 ±200, 用它算鼠标幅度信号
等于对着一个封顶的标签打分(`contract.ActionFrame` docstring / DataMiner
docs/45 §3)。A3 的带符号光流耦合必须能同时读这两路才能把 clamp 的影响单独
量出来。

## 布局

    <out>/<session_id>/<run_id>-<segment_id>/
        p0.mp4      符号链接到源 mp4(不重编码, 不复制 —— 单段 ~20MB × 857)
        p0.jsonl    逐帧一行, 与视频帧 1:1
        meta.json   合规戳 + video 块(fps_num/fps_den 必填, 否则契约拒收)
"""
from __future__ import annotations

import argparse
import hashlib
import json
from collections import defaultdict
from pathlib import Path

MOUSE_MOVE = "mouse_move"

RIGHTS_CHOICES = ("third_party", "first_party")


def compliance_stamp(rights: str, consent_id: str | None) -> dict:
    """两条车道的合规戳, 见模块 docstring。`production_certified` 两条都是 False。"""
    if rights not in RIGHTS_CHOICES:
        raise ValueError(f"unknown rights {rights!r}; expected {RIGHTS_CHOICES}")
    if rights == "third_party":
        if consent_id:
            raise ValueError(
                "third_party 车道不接受 consent-id —— 这批 run 的治理链本来就是空的; "
                "要走 owner 具结车道请显式传 --rights first_party")
        return {
            "rights": "local_debug_non_trainable",
            "first_party": False,
            "consent_id": None,
            "non_trainable": True,
            "is_trainable": False,
            "production_certified": False,
            "is_cutscene": False,
            "tags": ["lane_a_probe", "method_validation_only", "non_trainable"],
        }
    if not (consent_id or "").strip():
        raise ValueError(
            "--rights first_party 必须同时给 --consent-id(owner 具结 ID)。"
            "门是 first_party ∧ consent_id ∧ ¬cutscene ∧ is_trainable 四项与, "
            "只翻一个会让人以为改了其实没改。")
    return {
        "rights": "first_party",
        "first_party": True,
        "consent_id": consent_id.strip(),
        "non_trainable": False,
        "is_trainable": True,
        # 产品认证位: owner 具结说的是"验证用、不发布", 那不是产品认证
        "production_certified": False,
        "is_cutscene": False,
        # "不会在任何情况下发布和使用"这条约束必须留在数据上, 不能只留在对话里
        "tags": ["owner_attested_validation", "no_publish",
                 "no_product_training", "trainable"],
    }


def _load_tables(src: Path):
    import pyarrow.parquet as pq
    data = src / "data"
    return (
        pq.read_table(data / "segments.parquet"),
        pq.read_table(data / "frames.parquet"),
        pq.read_table(data / "events.parquet"),
    )


def _raw_pointer_by_frame(events_for_segment: list[dict],
                          frames_for_segment: list[dict]) -> dict[int, tuple[int, int]]:
    """未截断的逐帧指针位移: 按帧的 qpc 半开区间求和 mouse_move 的 dx/dy.

    帧窗口在段内首尾相接、无缝隙、每个窗口恰好一个帧周期(说明书 §4), 所以一次
    双指针扫过即可 —— 不需要对每帧重扫事件表。事件按 qpc 单调, 帧按 frame 单调。
    """
    moves = sorted((e["qpc"], e["dx"], e["dy"]) for e in events_for_segment
                   if e["type"] == MOUSE_MOVE)
    out: dict[int, tuple[int, int]] = {}
    i = 0
    for fr in sorted(frames_for_segment, key=lambda r: r["frame"]):
        lo, hi = fr["output_qpc_start"], fr["output_qpc_end"]
        while i < len(moves) and moves[i][0] < lo:
            i += 1
        j, sx, sy = i, 0, 0
        while j < len(moves) and moves[j][0] < hi:
            sx += moves[j][1]
            sy += moves[j][2]
            j += 1
        out[fr["frame"]] = (sx, sy)
    return out


def _segment_meta(seg: dict, n_frames: int, stamp: dict) -> dict:
    return {
        "sample_key": f'{seg["session_id"]}_{seg["run_id"]}-{seg["segment_id"]}',
        "session": seg["session_id"],
        "chunk": seg["segment_id"],
        "game": seg["game_id"],
        "platform": "pc",
        "schema_version": "dm-1",
        "key_schema": "dm-1 mapping (per-game physical vocab)",
        "annotation_source": "raw_input_recorded",
        "video": {
            "file": "p0.mp4",
            "codec": "h264",
            "width": seg["width"],
            "height": seg["height"],
            # fps 是承重字段: 契约拒绝"没声明 fps"的 chunk 而不是默认 30
            # (docs/50 P-4/P-8), 且必须给有理数真值 —— 30 与 29.97 都会被
            # 四舍五入成同一个整数 30, 那正是混合 fps 门要拦的东西
            "fps": float(seg["fps"]),
            "fps_num": int(round(float(seg["fps"]))),
            "fps_den": 1,
            "frames": n_frames,
            "audio": False,
        },
        "provenance": {
            "source": "DiogenesLab/Diogenes_COD_sample_v01 (local build)",
            "run_id": seg["run_id"],
            "segment_index": seg["segment_index"],
            "run_state": seg["run_state"],
            "run_stop_reason": seg["run_stop_reason"],
            "catalog_id": seg["catalog_id"],
            "catalog_version": seg["catalog_version"],
            "catalog_mode": seg["catalog_mode"],
            "mapping_id": seg["mapping_id"],
            "mapping_version": seg["mapping_version"],
            "video_sha256": seg["video_sha256"],
            "qpc_frequency": seg["qpc_frequency"],
            "run_anchor_qpc": seg["run_anchor_qpc"],
        },
        **stamp,
    }


def _sha256(path: Path, chunk: int = 1 << 20) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(chunk):
            h.update(block)
    return h.hexdigest()


def convert(src: Path, out: Path, *, per_session: int | None,
            sessions: list[str] | None, verify: int,
            rights: str = "third_party", consent_id: str | None = None) -> dict:
    stamp = compliance_stamp(rights, consent_id)
    segments, frames, events = _load_tables(src)

    frames_by_seg: dict[tuple, list[dict]] = defaultdict(list)
    for row in frames.to_pylist():
        frames_by_seg[(row["session_id"], row["run_id"], row["segment_id"])].append(row)
    events_by_seg: dict[tuple, list[dict]] = defaultdict(list)
    for row in events.to_pylist():
        events_by_seg[(row["session_id"], row["run_id"], row["segment_id"])].append(row)

    seen: dict[str, int] = defaultdict(int)
    written = verified = clamped = 0
    out.mkdir(parents=True, exist_ok=True)

    for seg in segments.to_pylist():
        sid = seg["session_id"]
        if sessions and sid not in sessions:
            continue
        if per_session is not None and seen[sid] >= per_session:
            continue
        seen[sid] += 1

        key = (sid, seg["run_id"], seg["segment_id"])
        rows = sorted(frames_by_seg[key], key=lambda r: r["frame"])
        if not rows:
            continue
        raw = _raw_pointer_by_frame(events_by_seg[key], rows)

        chunk_dir = out / sid / f'{seg["run_id"]}-{seg["segment_id"]}'
        chunk_dir.mkdir(parents=True, exist_ok=True)

        video_src = (src / seg["video_path"]).resolve()
        link = chunk_dir / "p0.mp4"
        if link.is_symlink() or link.exists():
            link.unlink()
        link.symlink_to(video_src)
        if verify and written < verify:
            if _sha256(video_src) != seg["video_sha256"]:
                raise SystemExit(f"sha256 mismatch: {video_src}")
            verified += 1

        with open(chunk_dir / "p0.jsonl", "w") as f:
            for r in rows:
                rdx, rdy = raw.get(r["frame"], (0, 0))
                if abs(rdx) > 200 or abs(rdy) > 200:
                    clamped += 1
                f.write(json.dumps({
                    "frame": r["frame"],
                    "gframe": r["session_frame"],
                    "video_t_ms": r["video_t_us"] // 1000,
                    "keys": list(r["keys"] or []),
                    "actions": list(r["actions"] or []),
                    # 转换器已 clamp 到 ±200, 是 pointerBins 的来源
                    "pointerDx": r["pointer_dx"],
                    "pointerDy": r["pointer_dy"],
                    # 从 events 半开区间重算的未截断真值
                    "pointerRawDx": rdx,
                    "pointerRawDy": rdy,
                    "pointerBins": list(r["pointer_bins"] or [0, 0]),
                    "duplicate": bool(r["duplicate"]),
                    "src": "rec",
                }) + "\n")

        meta = _segment_meta(seg, len(rows), stamp)
        (chunk_dir / "meta.json").write_text(json.dumps(meta, indent=1) + "\n")
        written += 1

    return {"chunks": written, "sessions": len(seen), "sha256_verified": verified,
            "frames_beyond_clamp": clamped, "rights": stamp["rights"],
            "is_trainable": stamp["is_trainable"], "tags": stamp["tags"]}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", type=Path, required=True,
                    help="COD 构建根目录(含 data/*.parquet 与 videos/)")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--max-segments-per-session", type=int, default=None,
                    dest="per_session")
    ap.add_argument("--sessions", nargs="+", default=None)
    ap.add_argument("--verify", type=int, default=2,
                    help="对前 N 个片段重算 sha256 与索引表对账(0 关闭)")
    ap.add_argument("--rights", default="third_party", choices=RIGHTS_CHOICES,
                    help="third_party(默认, 不可训练) / first_party(owner 具结车道, "
                         "必须同时给 --consent-id)")
    ap.add_argument("--consent-id", default=None,
                    help="owner 具结 ID, 只在 --rights first_party 下有效")
    a = ap.parse_args()
    stats = convert(a.src, a.out, per_session=a.per_session,
                    sessions=a.sessions, verify=a.verify,
                    rights=a.rights, consent_id=a.consent_id)
    print(json.dumps(stats, indent=1, ensure_ascii=False))
    print(f"→ {a.out}")
    if stats["is_trainable"]:
        print("⚠ owner 具结车道: 产物 is_trainable=true, 但带 no_publish / "
              "no_product_training 标签 —— 仅方法验证, 不得发布、不得进产品语料。")
    else:
        print("提醒: 产物全部 is_trainable=false, 只用于 Lane A 零训练体检。")


if __name__ == "__main__":
    main()
