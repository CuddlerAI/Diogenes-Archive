#!/usr/bin/env python3
"""三臂 E1a 逐 clip 分析 + 配对检验。

与 score_rollout 同一估计器/同一判据纪律（判据档 = 池化 p90 以下），只是把聚合
粒度从「帧对池」降到「clip」，使臂间可做配对检验（同一 holdout clip 集）。

用法（kda 上）:
  python perclip_e1a.py --arm A0:<mp4dir>:<orderfile> --arm A1:<dir>:<pairs.txt> ... \
      --actions <flat npz dir> --out perclip.json
orderfile: 每行一个 flat 名，第 i 行对应 rank0-<i>-0_regular.mp4（pairs.txt 是
"i\\tflat" 两列，也接受）。
"""
import argparse, json
from pathlib import Path

import numpy as np
from scipy import stats

import sys
sys.path.insert(0, str(Path.home() / "mlcode/ml"))
from diogenes_wm.wm01.score_rollout import (DEFAULT_DOWNSCALE, DEFAULT_MIN_DELTA,
                                            _coupling, _decode, _pairs)


def load_order(path: Path) -> dict[int, str]:
    lines = path.read_text().splitlines()
    out = {}
    for i, ln in enumerate(lines):
        if "\t" in ln:
            a, b = ln.split("\t")
            out[int(a)] = b
        else:
            out[i] = ln.strip()
    return out


def collect(mp4_dir: Path, order: dict[int, str], actions: Path):
    per = {}
    for i, flat in order.items():
        mp4 = mp4_dir / f"rank0-{i}-0_regular.mp4"
        npz = actions / f"{flat}.npz"
        if not mp4.exists() or not npz.exists():
            continue
        mouse = np.load(npz)["mouse"]
        est, lab = _pairs(_decode(mp4, DEFAULT_DOWNSCALE), mouse,
                          min_delta=DEFAULT_MIN_DELTA, downscale=DEFAULT_DOWNSCALE)
        if est.size:
            per[flat] = (est, lab)
    return per


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", action="append", required=True,
                    help="NAME:mp4dir:orderfile")
    ap.add_argument("--actions", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()

    arms = {}
    for spec in a.arm:
        name, d, o = spec.split(":")
        arms[name] = collect(Path(d), load_order(Path(o)), a.actions)
        print(f"[{name}] {len(arms[name])} clips")

    # 池化 p90（判据档阈值）：用各臂共有 clip 的动作幅度联合分布——动作同源，
    # 任一臂的 lab 池分布相同，取第一臂便可，但为对称起见取交集池。
    common = sorted(set.intersection(*[set(v) for v in arms.values()]))
    print(f"common clips: {len(common)}")
    lab_pool = np.concatenate([arms[next(iter(arms))][c][1] for c in common])
    p90 = float(np.percentile(np.abs(lab_pool).max(axis=1), 90))

    # 池化统计 + clip 级配对 bootstrap：统计量 = 判据档池化 r（无偏），
    # 重采样单位 = clip（同一重采样集应用到所有臂 → 配对）。
    def pooled(name, clips):
        est = np.concatenate([arms[name][c][0] for c in clips])
        lab = np.concatenate([arms[name][c][1] for c in clips])
        mag = np.abs(lab).max(axis=1)
        m = mag < p90
        return _coupling(est[m], lab[m])

    point = {n: pooled(n, common) for n in arms}
    rng = np.random.default_rng(0)
    B = 2000
    boot = {n: [] for n in arms}
    for _ in range(B):
        sample = [common[k] for k in rng.integers(0, len(common), len(common))]
        for n in arms:
            boot[n].append(pooled(n, sample)["coupling"])
    boot = {n: np.array(v) for n, v in boot.items()}

    tests = {}
    names = list(arms)
    for i in range(len(names)):
        for j in range(i + 1, len(names)):
            d = boot[names[i]] - boot[names[j]]
            tests[f"{names[i]}_vs_{names[j]}"] = {
                "point_diff": point[names[i]]["coupling"] - point[names[j]]["coupling"],
                "ci95": [float(np.percentile(d, 2.5)), float(np.percentile(d, 97.5))],
                "p_boot_two_sided": float(2 * min((d <= 0).mean(), (d >= 0).mean())),
            }

    rows = {n: {"pooled": point[n],
                "boot_ci95": [float(np.percentile(boot[n], 2.5)),
                              float(np.percentile(boot[n], 97.5))]} for n in arms}
    out = {"p90": p90, "n_common": len(common),
           "arm_pooled": rows,
           "tests": tests,
           "n_boot": 2000}
    a.out.write_text(json.dumps(out, ensure_ascii=False, indent=1))
    print(json.dumps({k: v for k, v in out.items() if k != "per_clip"},
                     ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main()
