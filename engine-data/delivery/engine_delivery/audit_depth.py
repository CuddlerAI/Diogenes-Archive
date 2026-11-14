"""判一次 depth 采集**能不能**转成 68 号交付形态，以及还差什么。

不是质检产物，是质检**原料**。`depth-lab deliver` 已经会把采集转成三件交付物，
但它要求调用方在命令行上给投影参数，并且拒绝 `assumed` 的标定出门（deliver.rs 开头
那段注释写得很清楚）。于是有一类失败它拦不住也不该由它拦：**采集当时就没记下
这些参数**，事后谁也补不回来——数据已经落盘，投影矩阵没跟着落。

这个审计就是把「事后补不回来」的那部分提前一步说清楚：拿到一份 run，先回答
「它有没有资格进 deliver」，再谈转出来的东西对不对。

判据：[68 号 §6.4 / §6.5](../../../DataMiner/docs/68-sfm-depth-data-requirements/README.md)。
"""

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import List, Optional, Sequence

OK = "OK"
BAD = "BAD"

#: 68 号 §6.4：depth 固定 720p
DEPTH_W, DEPTH_H = 1280, 720

#: 线性化必须的东西。`deliver` 的 `Projection{a,b,metric_scale}` 由它们推出来；
#: 少任何一个，`d → 米制 z` 的映射就不在数据里，只能靠人在命令行上猜。
PROJECTION_KEYS = ("projection", "near", "near_plane", "reverse_z", "proj_a", "proj_b")
SCALE_KEYS = ("metric_scale", "metric_scale_m")
#: 与 RGB 视频对齐必须的东西：光有 present_seq 只知道「第几次 Present」，
#: 不知道它落在视频的第几帧。68 号 §6.5 的对齐是逐帧硬判据。
#: `source_frame` 是索引 v2（PR #469）补的那一列——v1 采集没有它。
TIMEBASE_KEYS = ("source_frame", "qpc_ns", "timestamp_ns", "time_ns", "video_frame", "frame")


@dataclass
class DepthAudit:
    run: str
    rows: int = 0
    checks: List[tuple] = field(default_factory=list)   # (rule, status, evidence)

    def add(self, rule: str, status: str, evidence: str) -> None:
        self.checks.append((rule, status, evidence))

    @property
    def verdict(self) -> str:
        return BAD if any(s == BAD for _, s, _ in self.checks) else OK

    @property
    def blocking(self) -> List[str]:
        return [f"{r}: {e}" for r, s, e in self.checks if s == BAD]

    def to_dict(self) -> dict:
        return {"run": self.run, "rows": self.rows, "verdict": self.verdict,
                "checks": [{"rule": r, "status": s, "evidence": e}
                           for r, s, e in self.checks]}


def _gaps(seqs: Sequence[int]) -> List[tuple]:
    """返回 [(前一个存在的 seq, 后一个存在的 seq, 缺了几帧)]。

    只统计**真的少了帧**的断口。步长为奇数时 `(b-a)//2 - 1` 可能算出 0
    （例如 58→61），那不是断口而是 parity 已经坏了，交给 `depth.parity` 去报——
    在这里记一条「缺 0 帧」只会把断口计数注水。
    """
    out = []
    for a, b in zip(seqs, seqs[1:]):
        lost = (b - a) // 2 - 1
        if b - a > 2 and lost > 0:
            out.append((a, b, lost))
    return out


def _duplicates(seqs: Sequence[int]) -> List[int]:
    """出现不止一次的 present_seq。

    重复不是「多了一帧」这么轻——`deliver` 按 `depth_<偶数帧>` 命名产物，
    两行争同一个 present_seq 就是两份 payload 争同一个交付文件名，后写的把先写的
    盖掉，而索引里仍然是两行。这种数据交出去，帧数对不上却一处报错都没有。

    单靠 `_gaps` 看不出来：重复让相邻差变成 0，永远不会 `> 2`。
    """
    seen, dup = set(), []
    for s in seqs:
        if s in seen and s not in dup:
            dup.append(s)
        seen.add(s)
    return dup


def audit_index(run: str, rows: List[dict],
                pose_index: Optional[List[dict]] = None) -> DepthAudit:
    rep = DepthAudit(run=run, rows=len(rows))
    if not rows:
        rep.add("depth.index", BAD, "depth-index.jsonl 为空")
        return rep

    # --- 分辨率 ---
    sizes = {(r.get("width"), r.get("height")) for r in rows}
    if sizes == {(DEPTH_W, DEPTH_H)}:
        rep.add("depth.resolution", OK, f"全部 {DEPTH_W}x{DEPTH_H}")
    else:
        rep.add("depth.resolution", BAD,
                f"分辨率不唯一或不是 {DEPTH_W}x{DEPTH_H}：{sorted(sizes)}")

    # --- 帧序：偶数步进、无洞、不重复 ---
    seqs = sorted(int(r["present_seq"]) for r in rows if "present_seq" in r)
    if len(seqs) != len(rows):
        rep.add("depth.index", BAD,
                f"{len(rows) - len(seqs)} 行缺 present_seq——无法定位它是哪一帧")
    odd = [s for s in seqs if s % 2]
    if odd:
        rep.add("depth.parity", BAD,
                f"{len(odd)} 帧 present_seq 为奇数（68 号 §6.5 只取偶数帧）")
    else:
        rep.add("depth.parity", OK, f"{len(seqs)} 帧全为偶数 present_seq")

    dup = _duplicates(seqs)
    if dup:
        rep.add("depth.unique", BAD,
                f"{len(dup)} 个 present_seq 重复出现（首个 {dup[0]}）——"
                f"索引 {len(seqs)} 行只覆盖 {len(set(seqs))} 个不同帧；"
                "deliver 按 depth_<偶数帧> 命名，重复即两份 payload 争同一个交付文件名")
    else:
        rep.add("depth.unique", OK, f"{len(seqs)} 个 present_seq 互不重复")

    gaps = _gaps(seqs)
    if gaps:
        lost = sum(g[2] for g in gaps)
        worst = max(gaps, key=lambda g: g[2])
        rep.add("depth.continuity", BAD,
                f"{len(gaps)} 处断口、共缺 {lost} 帧（最长一处 present_seq "
                f"{worst[0]}→{worst[1]} 缺 {worst[2]} 帧）——"
                "交付要求 depth_N 与视频偶数帧一一对应，缺一帧即「没对齐」")
    else:
        rep.add("depth.continuity", OK,
                f"present_seq {seqs[0]}..{seqs[-1]} 步进 2 无断口")

    # --- 线性化参数：这是「事后补不回来」的那一条 ---
    keys = set()
    for r in rows[: min(len(rows), 256)]:
        keys |= set(r.keys())
    has_proj = [k for k in PROJECTION_KEYS if k in keys]
    has_scale = [k for k in SCALE_KEYS if k in keys]
    if has_proj and has_scale:
        rep.add("depth.linearizable", OK,
                f"索引带投影参数 {has_proj} 与尺度 {has_scale}")
    else:
        missing = []
        if not has_proj:
            missing.append(f"投影参数（{'/'.join(PROJECTION_KEYS[:3])} 任一）")
        if not has_scale:
            missing.append("metric_scale")
        rep.add("depth.linearizable", BAD,
                "索引缺" + "、".join(missing) + "——落盘的是未线性化的缓冲值，"
                "没有这些参数就得不到 68 号要的 View-space linear depth（米）；"
                "采集已结束，补不回来")

    # --- 时间基：与 RGB 帧对齐的前提 ---
    has_tb = [k for k in TIMEBASE_KEYS if k in keys]
    if has_tb:
        rep.add("depth.timebase", OK, f"索引带时间基 {has_tb}")
    else:
        rep.add("depth.timebase", BAD,
                "索引只有 present_seq，没有时间戳/视频帧号——无法证明 15fps 均匀，"
                "也无法与 RGB 视频逐帧对齐（68 号 §6.5 是硬质检项）")

    # --- 位姿旁车：deliver 出 action_camera.json 的前提 ---
    if pose_index is None:
        rep.add("depth.pose_sidecar", BAD,
                "无逐帧位姿旁车——交付物 2 action_camera.json 无从生成")
    else:
        have = {int(p["present_seq"]) for p in pose_index if "present_seq" in p}
        miss = [s for s in seqs if s not in have]
        if miss:
            rep.add("depth.pose_sidecar", BAD,
                    f"{len(miss)} 个 depth 帧没有对应位姿（首个 {miss[0]}）")
        else:
            rep.add("depth.pose_sidecar", OK, f"{len(seqs)} 帧位姿齐全")

    # --- payload 形态（提示项，不阻断：转换本来就是离线一步）---
    exts = {Path(str(r.get("path", ""))).suffix for r in rows}
    if exts == {".exr"}:
        rep.add("depth.payload", OK, "已是 EXR（交付格式，仍需按 68 号 §6.4 核通道/精度）")
    elif exts == {".f32"}:
        rep.add("depth.payload", OK,
                "原始 fp32 裸块——转交付需再走一遍 fp16 + trunc(×100)/100 + 单通道 EXR")
    else:
        rep.add("depth.payload", BAD, f"payload 扩展名不唯一：{sorted(exts)}")
    return rep


def audit_run(run_dir) -> DepthAudit:
    run_dir = Path(run_dir)
    idx = run_dir / "depth-index.jsonl"
    if not idx.exists():
        rep = DepthAudit(run=str(run_dir))
        rep.add("depth.index", BAD, f"缺 {idx.name}")
        return rep
    rows = [json.loads(line) for line in idx.read_text(encoding="utf-8").splitlines()
            if line.strip()]
    pose_path = run_dir / "pose-index.jsonl"
    poses = None
    if pose_path.exists():
        poses = [json.loads(line) for line in
                 pose_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    return audit_index(str(run_dir), rows, poses)


def render(rep: DepthAudit) -> str:
    lines = [f"== depth 采集可交付性审计 · {rep.run} · {rep.rows} 帧 =="]
    for rule, status, ev in rep.checks:
        lines.append(f"[{status}] {rule:<22} {ev}")
    lines.append(f"-- 判定 {rep.verdict}"
                 + ("（能进 deliver）" if rep.verdict == OK
                    else f"（{len(rep.blocking)} 条阻断）"))
    return "\n".join(lines)
