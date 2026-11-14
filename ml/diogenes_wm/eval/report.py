"""评测报告器：对数据集跑 E1 自检 + dummy 对照 rollout，出 markdown.

用法（数据侧自检，无模型也能跑）:
    python -m diogenes_wm.eval.report --root ../data_demo/dataset --out /tmp/eval_report.md

真模型接入: 实现 rollout.WorldModel 协议后传给 run(model=...)。
"""
from __future__ import annotations

import argparse
import subprocess
from pathlib import Path

from diogenes_phase0.data.dataset import scan_dataset, _load_frames
from diogenes_phase0.contract import load_actions

from .probes import view_probe, drift_probe
from .rollout import DummyLastFrameModel, DummyNoiseDriftModel, rollout


def _git_rev() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                              capture_output=True, text=True).stdout.strip() or "?"
    except OSError:
        return "?"


def run(root: Path, max_chunks: int = 6, model=None) -> str:
    records = [r for r in scan_dataset(root) if not r.meta.is_cutscene][:max_chunks]
    if not records:
        raise SystemExit(f"{root} 下没有可用 chunk")
    lines = [
        "# 评测报告（E1/E2 骨架）", "",
        f"- git: `{_git_rev()}` · root: `{root}` · chunks: {len(records)}",
        f"- 模型: {'真模型' if model is not None else 'dummy 对照(harness 自检)'}", "",
        "| 探针 | 样本 | 明细 | 分数 |", "|---|---|---|---|",
    ]
    for rec in records:
        frames = _load_frames(rec)
        actions = load_actions(rec.actions_path)[: len(frames)]
        # E1a: 真实录像自检(动作-画面耦合基线)
        vp = view_probe(frames, actions)
        lines.append(f"| E1a@真值 {rec.meta.sample_key} | {vp.n_pairs} | "
                     f"r_dx={vp.r_dx:+.3f} r_dy={vp.r_dy:+.3f} | {vp.coupling:.3f} |")
        # E2: 模型(或 dummy 对照) rollout vs 真值
        m = model if model is not None else DummyLastFrameModel()
        pred = rollout(m, frames[0], actions[: len(frames) - 1])
        dp = drift_probe(pred, frames[1:])
        tag = "模型" if model is not None else "dummy-last"
        lines.append(f"| E2@{tag} {rec.meta.sample_key} | T={dp.horizon} | "
                     f"collapse_at={dp.collapse_at} | 末帧 {dp.psnr_curve[-1]:.1f}dB |")
    if model is None:
        # 正对照: 噪声漂移模型必须被 E2 抓到
        frames = _load_frames(records[0])
        actions = load_actions(records[0].actions_path)[: len(frames) - 1]
        pred = rollout(DummyNoiseDriftModel(), frames[0], actions)
        dp = drift_probe(pred, frames[1:])
        lines.append(f"| E2@dummy-noise(正对照) {records[0].meta.sample_key} | "
                     f"T={dp.horizon} | collapse_at={dp.collapse_at} | 应非 None |")
    lines += ["", "> E3/E4/E6 需模型或人工环节；E1b/E1c 事件探针随 M0-b 落。"]
    return "\n".join(lines) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--root", required=True)
    ap.add_argument("--max-chunks", type=int, default=6)
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    report = run(Path(a.root), a.max_chunks)
    if a.out:
        Path(a.out).write_text(report)
        print(f"→ {a.out}")
    else:
        print(report)


if __name__ == "__main__":
    main()
