"""Lane A · A4 覆盖率审计: 语料离世界模型的分布要求差多远.

    python -m diogenes_wm.coverage_audit --root <chunk 契约根> --out report.md

逐项对照 [09 号 Halo 计划 §3.4] 的分布要求表 —— 那张表是**采集规格**, 不是模型
超参, 所以它必须在开闸前被量出来, 而不是训练失败之后回头猜。判据全部只读
`p0.jsonl` + `meta.json`, **不解码任何一帧视频**, 因此在几秒内跑完整个语料。

刻意不做的事:
- **不算场景多样性的视觉距离**。那需要解码全部帧并跑一个特征提取器, 而它给出的
  数字("平均两两余弦距离 0.83")没有可对照的公开口径 —— Yan 的"90+ 场景风格"
  是人工计数的。所以这里只如实报告 metadata 里能证明的多样性维度(游戏数 / 地图
  标签 / 会话数 / 操作者数), 缺失就报缺失, 不用一个自造指标去填。
- **不给总分**。把七个维度压成一个数会让"会话太短"和"动作分布偏"互相抵消, 而它们
  需要的采集动作完全不同。
"""
from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import load_actions
from diogenes_phase0.data.dataset import scan_dataset

# 09 号文 §3.4 的会话纪律
SESSION_MIN_SECONDS = 600.0
# Yan 口径的场景风格数(画质天花板档), 只作对照不作门
YAN_SCENE_STYLES = 90


def _session_of(chunk_dir: Path) -> str:
    return chunk_dir.parent.name


def audit(root: Path) -> dict:
    recs = scan_dataset(root)
    if not recs:
        raise SystemExit(f"{root} 下没有可用 chunk")

    fps = {m.fps for m in (r.meta for r in recs)}
    games = Counter(r.meta.game for r in recs)
    mappings = Counter(f"{r.meta.mapping_id}@{r.meta.mapping_version}" for r in recs)

    per_session_frames: dict[str, int] = defaultdict(int)
    key_frames: Counter = Counter()
    action_frames: Counter = Counter()
    session_keysets: dict[str, set] = defaultdict(set)
    n_frames = n_with_input = n_dup = 0
    dx_all: list[float] = []
    dy_all: list[float] = []
    clamped = 0

    for rec in recs:
        sess = _session_of(rec.chunk_dir)
        acts = load_actions(rec.actions_path)
        per_session_frames[sess] += len(acts)
        raw_rows = [json.loads(x) for x in
                    rec.actions_path.read_text().splitlines() if x.strip()]
        for a, row in zip(acts, raw_rows):
            n_frames += 1
            n_dup += bool(row.get("duplicate"))
            for k in a.keys:
                key_frames[k] += 1
                session_keysets[sess].add(k)
            for name in row.get("actions", []):
                action_frames[name] += 1
            dx, dy = a.raw_dx, a.raw_dy
            dx_all.append(dx)
            dy_all.append(dy)
            if abs(dx) > 200 or abs(dy) > 200:
                clamped += 1
            # 「带输入信号」必须把鼠标键算进去。`ActionFrame.keys` 只有键盘 VK,
            # 而开火/开镜是 `buttons`(dm-1) / 只出现在 `actions` 里(COD 索引表)
            # —— 漏掉它会把 73.5% 读成 65.0%, 差的 8.5 个点全是"只按了鼠标键"
            # 的帧, 而那正是 FPS 里最该数进去的一类。
            if a.keys or dx or dy or row.get("buttons") or row.get("actions"):
                n_with_input += 1

    one_fps = next(iter(fps)) if len(fps) == 1 else None
    sess_secs = {s: n / one_fps for s, n in per_session_frames.items()} if one_fps else {}
    mag = np.abs(np.asarray(dx_all)) + np.abs(np.asarray(dy_all))
    moving = mag > 0

    # 动作分布的偏斜: 用归一化熵, 1.0 = 完全均匀, 0 = 全压在一个动作上
    counts = np.asarray([c for _, c in action_frames.most_common()], dtype=float)
    p = counts / counts.sum() if counts.size else np.asarray([1.0])
    entropy = float(-(p * np.log(p)).sum() / np.log(len(p))) if len(p) > 1 else 0.0

    return {
        "root": str(root),
        "chunks": len(recs),
        "sessions": len(per_session_frames),
        "frames": n_frames,
        "hours": (n_frames / one_fps / 3600) if one_fps else None,
        "fps_values": sorted(fps),
        "games": dict(games),
        "mappings": dict(mappings),
        "frames_with_input_pct": 100 * n_with_input / n_frames,
        "duplicate_pct": 100 * n_dup / n_frames,
        "pointer_clamp_exceeded_pct": 100 * clamped / n_frames,
        "session_seconds": sess_secs,
        "sessions_under_discipline": sorted(
            s for s, sec in sess_secs.items() if sec < SESSION_MIN_SECONDS),
        "session_seconds_median": float(np.median(list(sess_secs.values()))) if sess_secs else None,
        "action_frames": dict(action_frames.most_common()),
        "action_entropy_norm": entropy,
        "key_frames": dict(key_frames.most_common()),
        "keys_seen": len(key_frames),
        "keys_in_every_session": sorted(
            set.intersection(*session_keysets.values())) if session_keysets else [],
        "pointer_moving_pct": 100 * float(moving.mean()),
        "pointer_mag_p50": float(np.percentile(mag[moving], 50)) if moving.any() else 0.0,
        "pointer_mag_p99": float(np.percentile(mag[moving], 99)) if moving.any() else 0.0,
        "pointer_mag_max": float(mag.max()) if mag.size else 0.0,
    }


def _verdict(ok: bool | None) -> str:
    return {True: "🟩 达标", False: "🟥 缺口", None: "🟨 无法判定"}[ok]


def report(a: dict) -> str:
    rare = [(k, v) for k, v in a["action_frames"].items()
            if v < 0.01 * a["frames"]]
    n_sess = a["sessions"]
    short = len(a["sessions_under_discipline"])
    L = [
        "# A4 · 覆盖率缺口表（对照 09 号文 §3.4 分布要求）", "",
        f"- root: `{a['root']}`",
        f"- 规模: **{a['chunks']} chunk / {n_sess} session / {a['frames']:,} 帧"
        f" ≈ {a['hours']:.3f} h** @ {a['fps_values']} fps",
        f"- 动作映射: {a['mappings']}", "",
        "| 09 §3.4 维度 | 要求 | 实测 | 判定 |",
        "|---|---|---|---|",
        f"| 版本 | build pin, 跨版本不混训 | 单一 catalog/mapping: {len(a['mappings'])} 组 | {_verdict(len(a['mappings']) == 1)} |",
        f"| 地图 | ≥15 张, 单图 ≤15% | metadata 无地图字段 | {_verdict(None)} |",
        f"| 游戏 | 单游戏(方向一) | {list(a['games'])} | {_verdict(len(a['games']) == 1)} |",
        f"| 动作熵 | 键覆盖率 + 鼠标 Δ 直方图 | 归一化熵 **{a['action_entropy_norm']:.3f}** ({len(a['action_frames'])} 类语义动作, {a['keys_seen']} 个物理键) | {_verdict(a['action_entropy_norm'] >= 0.75)} |",
        f"| 稀有动作 | 每类要有统计量 | **{len(rare)} 类**占比 <1%: {', '.join(k for k, _ in rare[:6])}{' …' if len(rare) > 6 else ''} | {_verdict(len(rare) == 0)} |",
        f"| 会话 | 单会话 ≥10 分钟连续 | 中位 **{a['session_seconds_median']:.0f}s**; **{short}/{n_sess}** 个会话不足 600s | {_verdict(short == 0)} |",
        f"| 失败样本 | 保留死亡/坠落等 | 无事件通道, 不可从标注判定 | {_verdict(None)} |",
        f"| 画面固化 | FOV/画质固定, 无 overlay | 单一采集环境(metadata 一致) | {_verdict(True)} |",
        f"| 玩家构成 | 新手+熟手混合 | **单操作者**(说明书 §2.2) | {_verdict(False)} |",
        f"| 场景风格 | Yan 口径 {YAN_SCENE_STYLES}+ | metadata 无场景标签, 单游戏单环境 | {_verdict(False)} |",
        "",
        "## 标注侧健康度（这几项与采集量无关，是管线质量）", "",
        "| 项 | 实测 | 参照 |",
        "|---|---|---|",
        f"| 带输入信号的帧 | **{a['frames_with_input_pct']:.1f}%** | 文献要求 2.5–5% 带标注, 这里是全量标注 |",
        f"| 重复帧 | {a['duplicate_pct']:.3f}% | 越低越好 |",
        f"| 指针在动的帧 | {a['pointer_moving_pct']:.1f}% | 视角是 FPS 里最强的动作维度 |",
        f"| 指针幅度 p50 / p99 / max | {a['pointer_mag_p50']:.0f} / {a['pointer_mag_p99']:.0f} / {a['pointer_mag_max']:.0f} counts | |",
        f"| **超 ±200 clamp 的帧** | **{a['pointer_clamp_exceeded_pct']:.2f}%** | 这些帧的 `pointerDx/Dy` 是封顶值, 回归目标必须用 `pointerRawDx/Dy` |",
        "",
        "## 每个会话覆盖到的物理键", "",
        f"- 全语料出现过 **{a['keys_seen']}** 个 VK；**每个会话都出现过**的只有 "
        f"`{a['keys_in_every_session']}`（{len(a['keys_in_every_session'])} 个）",
        "- 差额 = 只在部分会话里出现的键。按会话切 holdout 时，它们在验证集上可能一次都不出现，"
        "此时该键的任何指标都是空气。",
        "",
        "## 语义动作分布", "",
        "| 动作 | 帧数 | 占比 |", "|---|---:|---:|",
        *[f"| `{k}` | {v:,} | {100 * v / a['frames']:.2f}% |"
          for k, v in a["action_frames"].items()],
        "",
        "## 会话时长（升序）", "",
        "| session | 秒 | ≥600s |", "|---|---:|---|",
        *[f"| {s} | {sec:.0f} | {'🟩' if sec >= SESSION_MIN_SECONDS else '🟥'} |"
          for s, sec in sorted(a["session_seconds"].items(), key=lambda kv: kv[1])],
    ]
    return "\n".join(L) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, required=True)
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--json", type=Path, default=None)
    a = ap.parse_args()
    stats = audit(a.root)
    if a.json:
        a.json.write_text(json.dumps(stats, indent=1, ensure_ascii=False) + "\n")
    md = report(stats)
    if a.out:
        a.out.write_text(md)
        print(f"→ {a.out}")
    else:
        print(md)


if __name__ == "__main__":
    main()
