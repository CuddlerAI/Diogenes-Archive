"""WM 0.1 · chunk 契约 → LongLive 训练布局(video/ + caption/ + action/).

    python -m diogenes_wm.wm01.dataset --src <chunk 契约根> --out <训练平面> \
        --size 832x480 --clip-frames 93

## 底座那侧要什么(一手读 NVlabs/LongLive `utils/dataset.py::MultiVideoConcatDataset`)

    <data_dir>/video/<子目录>/*.mp4
    <data_dir>/caption/<子目录>/*.json

且帧数不是随便取的:

    total_frames = first_chunk_frames + N × subsequent_chunk_frames
    subsequent_chunk_frames = num_frame_per_block × temporal_compression_ratio  # 8×4 = 32
    first_chunk_frames      = 1 + (num_frame_per_block - 1) × temporal_compression_ratio  # 29

所以合法的 clip 长度是 **29, 61, 93, 125, …**(`independent_first_frame=False` 时)。
本模块默认 93 帧 —— 在 16 fps 下是 5.8 秒, 一个 10 秒源片段刚好切出一条多一点。

`action/` 是**我们加的旁挂**, 底座的 dataset 不认识它; 训练侧由
`wm01` 的 collate 按同名路径取。分开放而不是塞进 caption json, 是因为它是
`[T, 6]` + `[T, 2]` 的数组, json 里存会又大又慢。

## 30 fps → 16 fps 的三条纪律

底座的 `target_fps` 是 16, 我们的语料是 30。重采样有三个地方可以悄悄做错:

1. **按时间建栅格, 不按帧序号抽取。** 30/16 = 1.875 不是整数, 按序号抽会产生
   周期性抖动; 而 D2E 那边更极端(53.88/2 = 26.94)。这条纪律沿用
   `diogenes_phase0/data/d2e.py`。
2. **指针位移逐窗口求和, 绝不抽样。** `pointerRawDx/Dy` 是**位移**不是状态 ——
   抽样会压缩 delta 分布, 直接把鼠标信号做小。这是 D2E 适配器上写过的同一条,
   在这里同样是承重的。
3. **不上采样。** 源帧率低于目标时直接拒绝, 不插值 —— 插出来的帧没有对应的输入,
   而模型会把它学成"这一帧没动作"。

## 键盘通道存的是**覆盖比例**不是 0/1

一个 16 fps 的输出帧覆盖 1.875 个源帧。某个键只在其中一个源帧上按下时,
取 OR 会把它读成"整帧都按着"(对 WASD 这种按住几秒的无所谓, 对 `primary_attack`
这种点射会把持续时间放大近 2 倍)。所以这里存 **[0,1] 的覆盖比例**:
要二值就 `> 0`(等价于 OR), 要更严就 `>= 0.5`, 两种读法都还在。

## 按 **run** 拼接, 不按 chunk 各切各的

同一个 run 里的 10 秒段是**时间上连续**的(`segment_index` 顺序), 分开切会在每段
尾部丢掉切不满一个 clip 的部分。实测: 300 帧的段切 93 帧@16fps 的 clip
(= 175 个源帧)只出得来 1 条, **丢 41.7%** —— 在一份 2.36 小时的语料上这是实打实的损失。

所以本模块先把一个 run 的所有段用 ffmpeg concat demuxer **流拷贝**(不重编码,
所有段的编码参数一致)拼成一条时间线, 再从时间线上切 clip。附带好处是
**可以切出长于单段的 clip** —— 60 秒 rollout 评测需要的连续真值正是这个。

拼接的两条硬约束:
- 只拼 `(session, run)` 相同且 `segment_index` **连续**的段。中间缺号说明有段被
  判废(掉帧整段作废是采集侧的既定策略), 跨过缺口拼会把两段不连续的画面粘成一条,
  而模型会把那个跳变学成一次合法的转场。缺号处**断开**, 各自成一条时间线。
- 参差尾帧**丢弃不补零**(`readiness.py` 已把它列为阻断项: 补零读作"这几帧没动",
  与"我们不知道"不可区分), 且丢了多少必须报 —— 静默截断读起来会像"全都用上了"。
"""
from __future__ import annotations

import argparse
import json
import subprocess
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import (
    PHASE0_CONTRACT_DM1_ATTESTED, PHASE0_CONTRACT_MODES, gate_fps,
    gate_phase0_contract, load_actions,
)
from diogenes_phase0.data.dataset import scan_dataset

from .action_space import BASE_KEYBOARD_DIM, ActionSpace

# LongLive 的块结构常量(见模块 docstring)
NUM_FRAME_PER_BLOCK = 8
TEMPORAL_COMPRESSION = 4
FIRST_CHUNK_FRAMES = 1 + (NUM_FRAME_PER_BLOCK - 1) * TEMPORAL_COMPRESSION      # 29
SUBSEQUENT_CHUNK_FRAMES = NUM_FRAME_PER_BLOCK * TEMPORAL_COMPRESSION           # 32
# ⚠ 24 不是 16。16 是 `MultiVideoConcatDataset(target_fps=16)` 的**默认形参**,
# 不是模型的时间尺度; 模型的真值在 `utils/config.py::wan_default_config` 里:
#     "Wan2.2-TI2V-5B": {..., "fps": 24, "spatial_compression_ratio": 16, ...}
# 用 16 fps 的数据训 24 fps 的模型 = 把它学到的运动先验按 1.5 倍拉伸, 而这在
# 损失曲线上看不出来。**底座参数一律以模型 config 为准, 不以 dataset 类的默认形参为准。**
TARGET_FPS = 24
# 潜空间下采样比(832x480 → 52x30)。动作模块的 (tt, th, tw) 网格由它算出。
SPATIAL_COMPRESSION = 16


def legal_clip_lengths(max_frames: int = 400) -> list[int]:
    n, out = FIRST_CHUNK_FRAMES, []
    while n <= max_frames:
        out.append(n)
        n += SUBSEQUENT_CHUNK_FRAMES
    return out


def check_clip_frames(n: int) -> None:
    if n not in legal_clip_lengths(max(n, 400)):
        raise ValueError(
            f"clip 帧数 {n} 不是 LongLive 的合法块长; 合法值是 "
            f"{legal_clip_lengths(200)}… (29 + k×32)")


@dataclass
class ResampleResult:
    """一次 30→16 的重采样结果。`source_index[i]` 是第 i 个输出帧覆盖的源帧区间。"""
    source_index: list[tuple[int, int]]   # 半开区间 [lo, hi)
    out_frames: int


def build_time_grid(n_src: int, src_fps: float, dst_fps: float) -> ResampleResult:
    """按**时间**建目标栅格, 每个输出帧覆盖一段源帧的半开区间。

    拒绝上采样: 目标帧率高于源帧率时插出来的帧没有对应输入。
    """
    if dst_fps > src_fps + 1e-6:
        raise ValueError(
            f"拒绝上采样: 源 {src_fps} fps → 目标 {dst_fps} fps。"
            "插出来的帧没有对应的输入, 模型会把它学成'这一帧没动作'。")
    duration = n_src / src_fps
    n_dst = int(np.floor(duration * dst_fps))
    edges = np.arange(n_dst + 1) / dst_fps          # 秒
    idx = np.clip(np.round(edges * src_fps).astype(int), 0, n_src)
    spans = [(int(idx[i]), int(max(idx[i + 1], idx[i] + 1))) for i in range(n_dst)]
    return ResampleResult(source_index=spans, out_frames=n_dst)


def clip_grid(src_per_clip: int, src_fps: float,
              clip_frames: int) -> ResampleResult | None:
    """一条 clip 的重采样栅格, 截到 clip_frames; 凑不满时返回 None(丢弃该 clip)。

    convert 与 `ab_p0.bridge` 共用这一份实现: 伪标签臂与真值臂消费**同一批视频
    clip**, 切窗计划哪怕差一帧, 两臂的动作就与画面静默错位 —— 所以栅格只能算一次。
    """
    grid = build_time_grid(src_per_clip, src_fps, TARGET_FPS)
    if grid.out_frames < clip_frames:
        return None
    grid.source_index = grid.source_index[:clip_frames]
    grid.out_frames = clip_frames
    return grid


def encode_actions(actions, grid: ResampleResult, semantics: list[list[str]],
                   space: ActionSpace) -> dict[str, np.ndarray]:
    """把源帧率的动作压到目标栅格上。位移求和, 键盘取覆盖比例。"""
    kb = np.zeros((grid.out_frames, BASE_KEYBOARD_DIM), np.float32)
    mo = np.zeros((grid.out_frames, 2), np.float32)
    for i, (lo, hi) in enumerate(grid.source_index):
        span = range(lo, min(hi, len(actions)))
        n = max(len(span), 1)
        for t in span:
            a = actions[t]
            # 位移: 求和(不是均值也不是抽样)
            mo[i, 0] += a.raw_dx
            mo[i, 1] += a.raw_dy
            kb[i] += space.keyboard_multihot(semantics[t], a.keys)
        kb[i] /= n                                   # → [0,1] 的覆盖比例
    return {"keyboard": kb, "mouse": mo}


def _ffmpeg_clip(src_mp4: Path, dst_mp4: Path, *, start_frame: int, n_src: int,
                 out_frames: int, src_fps: float, size: tuple[int, int],
                 crf: int) -> None:
    """从时间线上切一条 clip 并重采样到 TARGET_FPS。

    ⚠ `-frames:v` 数的是**输出**帧, 不是输入帧。写成源帧数会得到一条
    `n_src` 帧 @16fps 的视频(时长直接错成 30/16 = 1.875 倍), 而 ffprobe 之外
    没有任何地方会报错 —— 动作序列有 93 帧、视频有 175 帧, 训练侧按较短的那个
    截断, 于是画面和动作**静默错位**。所以这里显式传 out_frames, 并用 `-t`
    限制读入的时长。
    """
    w, h = size
    dst_mp4.parent.mkdir(parents=True, exist_ok=True)
    cmd = [
        "ffmpeg", "-v", "error", "-y",
        "-ss", f"{start_frame / src_fps:.6f}",
        "-t", f"{n_src / src_fps:.6f}",
        "-i", str(src_mp4),
        # fps 滤镜按时间重采样(丢帧/复帧), 与 build_time_grid 同一口径
        "-vf", f"fps={TARGET_FPS},scale={w}:{h}:flags=area",
        "-frames:v", str(out_frames),
        "-an", "-c:v", "libx264", "-crf", str(crf), "-pix_fmt", "yuv420p",
        "-movflags", "+faststart", str(dst_mp4),
    ]
    subprocess.run(cmd, check=True)


@dataclass
class Run:
    """一条时间上连续的段序列(同 session/run, segment_index 连号)。"""
    session: str
    run_id: str
    records: list          # ChunkRecord, 按 segment_index 排序


def group_runs(recs: list) -> list[Run]:
    """按 (session, run) 分组并在 segment_index 缺号处断开。

    缺号说明中间有段被判废(掉帧整段作废)。跨过缺口拼会把两段不连续的画面粘成一条,
    而模型会把那个跳变学成一次合法的转场 —— 所以缺口处断开, 各自成一条时间线。
    """
    buckets: dict[tuple[str, str], list] = {}
    for r in recs:
        meta = json.loads((r.chunk_dir / "meta.json").read_text())
        prov = meta.get("provenance") or {}
        run_id = str(prov.get("run_id", "r0000"))
        idx = int(prov.get("segment_index", 0))
        buckets.setdefault((r.chunk_dir.parent.name, run_id), []).append((idx, r))
    runs: list[Run] = []
    for (session, run_id), items in sorted(buckets.items()):
        items.sort(key=lambda kv: kv[0])
        cur, prev = [], None
        for idx, rec in items:
            if prev is not None and idx != prev + 1:
                runs.append(Run(session, run_id, cur))
                cur = []
            cur.append(rec)
            prev = idx
        if cur:
            runs.append(Run(session, run_id, cur))
    return runs


def _concat_run(run: Run, work: Path) -> Path:
    """把一个 run 的段流拷贝拼成一条时间线(不重编码)。"""
    work.mkdir(parents=True, exist_ok=True)
    listing = work / f"{run.session}-{run.run_id}.txt"
    listing.write_text("".join(
        f"file '{r.frames_path.resolve()}'\n" for r in run.records))
    joined = work / f"{run.session}-{run.run_id}.mp4"
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0",
         "-i", str(listing), "-c", "copy", str(joined)], check=True)
    return joined


def convert(src: Path, out: Path, *, size: tuple[int, int], clip_frames: int,
            caption: str, crf: int = 20, limit: int | None = None,
            space: ActionSpace | None = None, work: Path | None = None,
            contract_mode: str = PHASE0_CONTRACT_DM1_ATTESTED,
            require_declared_source: bool = False,
            expected_fps: float | None = None) -> dict:
    check_clip_frames(clip_frames)
    space = space or ActionSpace()
    recs = scan_dataset(src)
    if not recs:
        raise SystemExit(f"{src} 下没有可用 chunk")
    # 合规门在**这里**跑, 因为这一步才是"字节进训练"的边界: 上一步(适配器)盖的戳
    # 是自陈的, 谁也没规定下一个人一定会去看。默认 dm1-attested = 具结车道:
    # 治理门全开、不放宽。要拿方法验证语料建平面就显式说 --contract-mode。
    metas = [r.meta for r in recs]
    gate_phase0_contract(metas, contract_mode=contract_mode)
    if require_declared_source:
        from diogenes_phase0.contract import gate_sources  # noqa: PLC0415
        gate_sources(metas, allow_method_validation_only=False,
                     require_declared_source=True)
    # 语料级混合帧率: convert 自己只查一条 run 内部, 而窗口长度/指针幅度都是按帧计的
    gate_fps(metas, expected_fps=expected_fps)
    runs = group_runs(recs)[: limit or None]
    work = work or (out / "_work")

    written = dropped_tail_frames = skipped_short = total_src = 0
    for run in runs:
        fps_values = {r.meta.fps for r in run.records}
        if len(fps_values) != 1:
            raise ValueError(
                f"{run.session}/{run.run_id} 混合帧率 {sorted(fps_values)} —— "
                "一条时间线只能有一个帧率")
        src_fps = fps_values.pop()

        acts, sems = [], []
        for r in run.records:
            acts.extend(load_actions(r.actions_path))
            sems.extend(json.loads(x).get("actions", []) for x in
                        r.actions_path.read_text().splitlines() if x.strip())
        n_src_total = min(len(acts), len(sems))
        total_src += n_src_total
        src_per_clip = int(np.ceil(clip_frames * src_fps / TARGET_FPS))
        n_clips = n_src_total // src_per_clip
        if n_clips == 0:
            skipped_short += 1
            dropped_tail_frames += n_src_total
            continue
        dropped_tail_frames += n_src_total - n_clips * src_per_clip

        joined = _concat_run(run, work)
        for c in range(n_clips):
            lo = c * src_per_clip
            grid = clip_grid(src_per_clip, src_fps, clip_frames)
            if grid is None:
                skipped_short += 1
                continue
            enc = encode_actions(acts[lo:lo + src_per_clip], grid,
                                 sems[lo:lo + src_per_clip], space)
            stem = f"{run.run_id}-{c:04d}"
            _ffmpeg_clip(joined, out / "video" / run.session / f"{stem}.mp4",
                         start_frame=lo, n_src=src_per_clip,
                         out_frames=clip_frames, src_fps=src_fps,
                         size=size, crf=crf)
            cap_p = out / "caption" / run.session / f"{stem}.json"
            cap_p.parent.mkdir(parents=True, exist_ok=True)
            cap_p.write_text(json.dumps({"caption": caption}, ensure_ascii=False))
            act_p = out / "action" / run.session / f"{stem}.npz"
            act_p.parent.mkdir(parents=True, exist_ok=True)
            np.savez_compressed(act_p, **enc)
            written += 1

    stats = {
        "runs": len(runs),
        "clips": written,
        "clip_frames": clip_frames,
        "clip_seconds": round(clip_frames / TARGET_FPS, 2),
        "target_fps": TARGET_FPS,
        "size": f"{size[0]}x{size[1]}",
        "vocab_fingerprint": space.fingerprint,
        # 静默截断读起来会像"全都用上了" —— 丢了多少必须报
        "dropped_tail_frames": dropped_tail_frames,
        "dropped_pct": round(100 * dropped_tail_frames / max(total_src, 1), 2),
        "runs_too_short": skipped_short,
    }
    out.mkdir(parents=True, exist_ok=True)
    (out / "wm01_dataset.json").write_text(
        json.dumps(stats, indent=1, ensure_ascii=False) + "\n")
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--size", default="832x480",
                    help="必须是 %d 的整数倍(潜空间下采样比)" % SPATIAL_COMPRESSION)
    ap.add_argument("--clip-frames", type=int, default=93)
    ap.add_argument("--crf", type=int, default=20)
    ap.add_argument("--limit", type=int, default=None, help="只取前 N 条 run")
    ap.add_argument("--caption", default="first-person shooter gameplay footage")
    ap.add_argument("--contract-mode", default=PHASE0_CONTRACT_DM1_ATTESTED,
                    choices=sorted(PHASE0_CONTRACT_MODES),
                    help="这条平面走哪条车道(默认 dm1-attested = 具结车道, 治理门"
                         "全开)。方法验证语料要显式说 —— 默认放宽等于没有门")
    ap.add_argument("--require-declared-source", action="store_true",
                    help="要求每个 chunk 都带来源坐标(contract.SOURCE_RULES 的判据)。"
                         "新车道建议打开; 既有语料(data_demo/gaming-500/D2E/cod)"
                         "落盘时还没有这两个字段")
    ap.add_argument("--expected-fps", type=float, default=None,
                    help="把整条语料钉在这个帧率上(不给则只要求语料内部同质)")
    a = ap.parse_args()
    w, h = (int(x) for x in a.size.split("x"))
    stats = convert(a.src, a.out, size=(w, h), clip_frames=a.clip_frames,
                    caption=a.caption, crf=a.crf, limit=a.limit,
                    contract_mode=a.contract_mode,
                    require_declared_source=a.require_declared_source,
                    expected_fps=a.expected_fps)
    print(json.dumps(stats, indent=1, ensure_ascii=False))
    print(f"→ {a.out}")


if __name__ == "__main__":
    main()
