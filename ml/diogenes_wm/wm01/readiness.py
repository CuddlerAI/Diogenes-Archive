"""WM 0.1 · 语料就绪门: 在租 GPU 之前拦下那些买不到判据的划分.

    python -m diogenes_wm.wm01.readiness --root <chunk 契约根> --holdout-sessions 2

这道门存在的理由是 research/33 §11 A4 的一条实测: COD 语料里 **22 个物理键, 而
「每个会话都出现过」的是 0 个**。按会话切 holdout 时, 某个动作通道在验证集上可以
一次都不出现 —— 此时该通道的动作遵循指标不是"低", 是**空气**, 而它照样会被打印成
一个数字。IDM 线的 `key_macro_f1_common` 用 ≥1% 的语料级支持度挡过同类问题, 但那道
防线设在**语料**级, 拦不住**会话**级的零支持。

门检查三件事, 每件都能独立让一次训练白跑:

1. **每个键盘通道在 train 与 holdout 上都要有支持**, 且 holdout 支持度不低于阈值。
2. **train 与 holdout 的动作分布不能差太远**。COD 实测: 按会话切 2 个 holdout 后,
   `combat.aim` 在 train 是 23.09% 而在 holdout 只有 6.87%(**0.30 倍**),
   `move.forward` 则从 35.41% 涨到 64.99%(1.84 倍)。19 个会话、单操作者的语料里,
   一个会话就是一段**打法**, 所以会话级 holdout 量的是"换了种打法还行不行",
   不是"泛化"。不并列报这个比值, 就会把打法差异读成模型差异。
3. **鼠标通道要有足够的运动帧** —— 视角是 FPS 里最强的动作维度, 静止语料训不出
   动作遵循, 而这在损失曲线上看不出来。
4. **chunk 帧数要能整除 VAE 的时间压缩比** —— 否则切窗在尾部补零, 而补零读作
   "这几帧没动", 与"我们不知道"不可区分。正确修法是**丢掉参差的尾帧**, 不是补零。

**只报告不修**: 门失败给出的动作是改采集或改划分, 不是放宽阈值。

## 一个例外: 语料**自己声明**被 mask 掉的通道

Halo 语料在转换期有意剔掉了 `combat.aim`(toggle-zoom 是坏标签, docs/50 §4.2),
于是该通道在每一帧上恒为 0。这与"这个通道在语料里没出现"是**两件事**: 前者是我们
自己下的裁决, 后者是数据缺陷。判据只有一个 —— chunk 的 `meta.masked_actions`
(`ChunkMeta.masked_action_semantics`)。所以:

* **每个 chunk 都声明 mask 了该通道** → 记为"已知空", 不算阻断, 但在报告里单列;
* **只有一部分 chunk 声明了** → **阻断**。同一个通道在半份语料里是标签、在另半份
  里被拿掉了, 拼起来的分布是假的, 而这在任何一项统计上都看不出来;
* **没声明 mask 却零支持** → 照旧阻断(那才是 A4 那条教训)。

另: mask 掉一个通道并不改变词表长度(底座 `keyboard_dim_in=6` 是硬预算, 落地是
"第 5 位恒 0"), 所以 `vocab_fingerprint` 与 COD 臂**逐字节相同** —— 两份 checkpoint
会对外声称同一个动作空间而其中一个的 aim 通道是死的。本模块因此另算一个
`effective_fingerprint`(把被 mask 的通道名打上 `#masked` 再取指纹)并要求它随
checkpoint 一起落盘。真正的修法是新增一份带独立指纹的冻结词表, 那要改
`action_space.py` 的冻结表, 不在这道门里做。
"""
from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

from diogenes_phase0.contract import load_actions
from diogenes_phase0.data.dataset import scan_dataset

from .action_space import (
    BASE_VAE_TIME_COMPRESSION, ActionSpace, vocab_fingerprint,
)

# holdout 上低于这个支持度的通道, 其指标不可解读
MIN_HOLDOUT_FRAMES = 30
# 鼠标运动帧占比下限(COD 实测 43.7%, D2E 的键盘类游戏会低得多)
MIN_MOVING_FRACTION = 0.05
# holdout/train 的通道占比之比落在这个区间外, 该通道的 holdout 指标测的是打法差异
SHIFT_BAND = (0.5, 2.0)


def _session_of(chunk_dir: Path) -> str:
    return chunk_dir.parent.name


def check(root: Path, *, holdout_sessions: int = 2,
          space: ActionSpace | None = None) -> dict:
    space = space or ActionSpace()
    recs = scan_dataset(root)
    if not recs:
        raise SystemExit(f"{root} 下没有可用 chunk")

    sessions = sorted({_session_of(r.chunk_dir) for r in recs})
    if holdout_sessions >= len(sessions):
        raise SystemExit(
            f"holdout 要 {holdout_sessions} 个会话, 语料只有 {len(sessions)} 个")
    # 会话 id 升序取尾, 与 MV 的 split_rule 同一口径
    holdout = set(sessions[-holdout_sessions:]) if holdout_sessions else set()

    # 语料自己声明的 mask: 全体 chunk 都声明的才算"已知空", 只有一部分声明 =
    # 同一个通道在半份语料里是标签、在另半份里被拿掉了(见模块 docstring)
    declared = [set(r.meta.masked_action_semantics) for r in recs]
    masked_all = set.intersection(*declared) if declared else set()
    masked_any = set.union(*declared) if declared else set()
    masked_partial = masked_any - masked_all

    per_split: dict[str, Counter] = {"train": Counter(), "holdout": Counter()}
    frames = {"train": 0, "holdout": 0}
    moving = {"train": 0, "holdout": 0}
    mags: dict[str, list[float]] = defaultdict(list)
    window_misaligned: list[str] = []

    for rec in recs:
        split = "holdout" if _session_of(rec.chunk_dir) in holdout else "train"
        if rec.n_frames % BASE_VAE_TIME_COMPRESSION != 0:
            window_misaligned.append(rec.meta.sample_key)
        acts = load_actions(rec.actions_path)
        raw = [json.loads(x) for x in
               rec.actions_path.read_text().splitlines() if x.strip()]
        for a, row in zip(acts, raw):
            frames[split] += 1
            kb = space.keyboard_multihot(row.get("actions"), a.keys)
            for i, on in enumerate(kb):
                if on:
                    per_split[split][space.keyboard[i]] += 1
            m = abs(a.raw_dx) + abs(a.raw_dy)
            if m > 0:
                moving[split] += 1
                mags[split].append(m)

    channels = []
    for name in space.keyboard:
        tr, ho = per_split["train"][name], per_split["holdout"][name]
        tr_pct = 100 * tr / max(frames["train"], 1)
        ho_pct = 100 * ho / max(frames["holdout"], 1)
        shift = (ho_pct / tr_pct) if tr_pct > 0 else None
        masked = name in masked_all
        channels.append({
            "channel": name,
            "train_frames": tr,
            "holdout_frames": ho,
            "train_pct": tr_pct,
            "holdout_pct": ho_pct,
            "shift": shift,
            "shift_ok": (shift is not None
                         and SHIFT_BAND[0] <= shift <= SHIFT_BAND[1]),
            # 语料自己声明剔掉的通道: 它是"已知空", 不是缺陷
            "masked": masked,
            "ok": masked or (tr > 0 and ho >= MIN_HOLDOUT_FRAMES),
        })

    mv = {s: (moving[s] / frames[s] if frames[s] else 0.0)
          for s in ("train", "holdout")}
    blockers, warnings = [], []
    if masked_partial:
        blockers.append(
            f"语料的 mask 口径不一致: {sorted(masked_partial)} 只在**一部分** chunk "
            f"里被剔除({len([d for d in declared if masked_partial & d])}/{len(recs)} "
            "个 chunk 声明了) —— 同一个通道在半份语料里是标签、在另半份里被拿掉了, "
            "拼起来的分布是假的, 而这在任何一项统计上都看不出来。要么全 mask 要么"
            "全不 mask, 分开跑两条臂也行")
    for c in channels:
        if c["masked"]:
            # 语料**自己声明**剔掉的通道: 不是数据缺陷, 是我们下的裁决。这里既不
            # 阻断也不假装它有支持 —— 单列出来, 让报数的人看得见这一列是空的。
            if c["train_frames"] or c["holdout_frames"]:
                warnings.append(
                    f"通道 `{c['channel']}` 声明被 mask, 却仍有 "
                    f"{c['train_frames'] + c['holdout_frames']} 帧带着它 —— "
                    "mask 没落干净, 查 p0.jsonl 的 actions 列")
            continue
        if c["train_frames"] == 0:
            blockers.append(f"通道 `{c['channel']}` 在 train 上零支持 —— 学不到")
        elif c["holdout_frames"] < MIN_HOLDOUT_FRAMES:
            blockers.append(
                f"通道 `{c['channel']}` 在 holdout 上只有 {c['holdout_frames']} 帧"
                f"(<{MIN_HOLDOUT_FRAMES}) —— 该通道的动作遵循指标不可解读")
        elif not c["shift_ok"]:
            # 分布漂移不阻断(会话级 holdout 上它几乎必然发生), 但必须并列报出来,
            # 否则打法差异会被读成模型差异
            warnings.append(
                f"通道 `{c['channel']}` 的 holdout/train 占比之比 "
                f"{c['shift']:.2f}× 落在 {SHIFT_BAND} 之外 "
                f"({c['train_pct']:.2f}% → {c['holdout_pct']:.2f}%) —— "
                "该通道的 holdout 指标里混着打法差异")
    for s in ("train", "holdout"):
        if mv[s] < MIN_MOVING_FRACTION:
            blockers.append(
                f"{s} 的指针运动帧只有 {100 * mv[s]:.1f}% "
                f"(<{100 * MIN_MOVING_FRACTION:.0f}%) —— 视角是最强的动作维度, "
                "这份划分训不出动作遵循")
    if window_misaligned:
        blockers.append(
            f"{len(window_misaligned)} 个 chunk 的帧数不是 "
            f"{BASE_VAE_TIME_COMPRESSION} 的整数倍(如 {window_misaligned[:3]}) "
            "—— 切窗会在尾部补零, 而补零读作'这几帧没动'")

    # mask 不改变词表长度(底座 keyboard_dim_in=6 是硬预算), 所以基指纹与 COD 臂
    # 逐字节相同 —— 两份 checkpoint 会声称同一个动作空间而其中一个通道是死的。
    effective_vocab = tuple(f"{n}#masked" if n in masked_all else n
                            for n in space.keyboard)
    return {
        "root": str(root),
        "vocab": {"name": space.name, "version": space.version,
                  "fingerprint": space.fingerprint,
                  "effective_fingerprint": vocab_fingerprint(effective_vocab),
                  "masked_channels": sorted(masked_all),
                  "channels": list(space.keyboard)},
        "sessions": {"total": len(sessions), "holdout": sorted(holdout)},
        "frames": frames,
        "channels": channels,
        "pointer_moving_fraction": mv,
        "pointer_mag_p99": {
            s: (float(np.percentile(mags[s], 99)) if mags[s] else 0.0)
            for s in ("train", "holdout")},
        "blockers": blockers,
        "warnings": warnings,
        "ready": not blockers,
    }


def _channel_row(c: dict) -> str:
    shift = "—" if c["shift"] is None else f"{c['shift']:.2f}×"
    if not c["shift_ok"] and not c.get("masked"):
        shift += " ⚠"
    verdict = "⬜ 已 mask" if c.get("masked") else ("🟩" if c["ok"] else "🟥")
    return (f"| `{c['channel']}` | {c['train_frames']:,} | {c['train_pct']:.2f}% | "
            f"{c['holdout_frames']:,} | {c['holdout_pct']:.2f}% | {shift} | "
            f"{verdict} |")


def report(r: dict) -> str:
    L = [
        "# WM 0.1 · 语料就绪门", "",
        f"- root: `{r['root']}`",
        f"- 词表: `{r['vocab']['name']}/{r['vocab']['version']}` "
        f"fingerprint `{r['vocab']['fingerprint']}`",
        *([f"- ⬜ 语料声明 mask 掉的通道: "
           f"{', '.join('`' + c + '`' for c in r['vocab']['masked_channels'])} "
           f"—— 该通道恒为 0, 相关指标是**空气**不是低分; 基指纹与未 mask 的臂"
           f"逐字节相同, 落 checkpoint 时必须记 effective_fingerprint "
           f"`{r['vocab']['effective_fingerprint']}`"]
          if r["vocab"]["masked_channels"] else []),
        f"- 划分: {r['sessions']['total']} 个会话, holdout = {r['sessions']['holdout']}",
        f"- 帧数: train {r['frames']['train']:,} / holdout {r['frames']['holdout']:,}",
        "",
        "| 键盘通道 | train 帧 | train % | holdout 帧 | holdout % | 漂移 | 判定 |",
        "|---|---:|---:|---:|---:|---:|---|",
        *[_channel_row(c) for c in r["channels"]],
        "",
        f"- 指针运动帧: train {100 * r['pointer_moving_fraction']['train']:.1f}% / "
        f"holdout {100 * r['pointer_moving_fraction']['holdout']:.1f}%",
        f"- 指针幅度 p99: train {r['pointer_mag_p99']['train']:.0f} / "
        f"holdout {r['pointer_mag_p99']['holdout']:.0f} counts",
        "",
    ]
    if r["ready"]:
        L.append("**🟩 就绪。** 每个通道在两侧都有支持, 指标可解读。")
    else:
        L.append(f"**🟥 未就绪, {len(r['blockers'])} 项阻断:**")
        L += [f"{i}. {b}" for i, b in enumerate(r["blockers"], 1)]
        L += ["", "> 门失败的动作是**改采集或改划分**, 不是放宽阈值。"]
    if r.get("warnings"):
        L += ["", f"**⚠ {len(r['warnings'])} 项分布漂移(不阻断, 但报数时必须并列):**"]
        L += [f"{i}. {w}" for i, w in enumerate(r["warnings"], 1)]
    return "\n".join(L) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, required=True)
    ap.add_argument("--holdout-sessions", type=int, default=2)
    ap.add_argument("--out", type=Path, default=None)
    a = ap.parse_args()
    r = check(a.root, holdout_sessions=a.holdout_sessions)
    md = report(r)
    if a.out:
        a.out.write_text(md)
        print(f"→ {a.out}")
    print(md)
    raise SystemExit(0 if r["ready"] else 2)


if __name__ == "__main__":
    main()
