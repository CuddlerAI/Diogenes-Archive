"""WM 0.1 · 给 rollout 打分: E1a 视角遵循(分位移档) + E2 漂移.

    python -m diogenes_wm.wm01.score_rollout --rollouts <生成的 mp4 目录> \
        --actions <plane>/action --out report.md [--control <不喂动作的 rollout 目录>]

每个阶段(Phase 0 基线 / Phase 1 / Phase 2 / 各 scale arm)都用这一个入口打分,
**指标定义因此只有一份** —— 两份实现会漂移, 而漂移之后跨阶段的数字就不再可比。

## 三条报数纪律, 都是硬编码进输出的, 不是提醒

1. **分位移档报 E1a。** Lane A 实测: 未截断信号的 `r_dx`(−0.518)反而比封顶的
   (−0.550)**更低** —— 甩枪帧的位移超出 Farneback 的搜索范围, **大位移上瓶颈是
   量具不是标签**。所以 `|Δ| ≥ p90` 那一档只作观察项, 判据只看 `< p90` 档。
   不分档就会把估计器的失效读成模型的失败。
2. **与真值上界并列。** 同一估计器在**真实录像**上是 **0.566**(BioShock 基线 0.55–0.60)。
   生成画面不可能比真实录像更容易被光流读出动作, 所以这是天花板。
   判据是**占真值的比例**, 不是绝对阈值。
3. **与不喂动作的对照并列。** 一个只会"画面在动"的模型也能拿到非零耦合
   —— 因为镜头本来就在动。没有对照的 E1a 读数不能用。

## 不做的事

**不给总分。** 把"画面像不像"和"动作跟不跟随"压成一个数, 会让 Phase 1 的成功
掩盖 Phase 2 的失败 —— 而那两件事需要的动作完全不同(一个改配方, 一个加数据)。
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

# Lane A 实测的真值上界: 同一估计器在真实录像上的耦合(research/33 §11 A3-b)
GROUND_TRUTH_COUPLING = 0.566
DEFAULT_MIN_DELTA = 3.0
DEFAULT_DOWNSCALE = 4


def _decode(path: Path, downscale: int) -> np.ndarray:
    import cv2
    cap = cv2.VideoCapture(str(path))
    frames = []
    ok, f = cap.read()
    while ok:
        frames.append(cv2.cvtColor(f, cv2.COLOR_BGR2RGB)[::downscale, ::downscale])
        ok, f = cap.read()
    cap.release()
    if not frames:
        raise SystemExit(f"{path} 解不出帧")
    return np.stack(frames).astype(np.float32).mean(axis=-1)


def _pairs(gray: np.ndarray, mouse: np.ndarray, *, min_delta: float,
           downscale: int) -> tuple[np.ndarray, np.ndarray]:
    """(估计位移, 标签位移) 对。估计器与 IDM/WM 两条线共用, 刻意不另写一份。"""
    from diogenes_wm.eval.probes import _flow_shift, phase_shift
    est, lab = [], []
    n = min(len(gray), len(mouse))
    for t in range(n - 1):
        dx, dy = float(mouse[t, 0]), float(mouse[t, 1])
        if abs(dx) < min_delta and abs(dy) < min_delta:
            continue          # 静止时光流是噪声, 拿噪声去相关会制造一个数字
        shift = _flow_shift(gray[t], gray[t + 1]) or phase_shift(gray[t], gray[t + 1])
        est.append((shift[0] * downscale, shift[1] * downscale))
        lab.append((dx, dy))
    return np.asarray(est), np.asarray(lab)


def _r(x: np.ndarray, y: np.ndarray) -> float:
    if x.size < 3 or x.std() == 0 or y.std() == 0:
        return 0.0
    return float(np.corrcoef(x, y)[0, 1])


def _coupling(est: np.ndarray, lab: np.ndarray) -> dict:
    if est.size == 0:
        return {"pairs": 0, "r_dx": 0.0, "r_dy": 0.0, "coupling": 0.0}
    r_dx, r_dy = _r(est[:, 0], lab[:, 0]), _r(est[:, 1], lab[:, 1])
    return {"pairs": int(len(est)), "r_dx": r_dx, "r_dy": r_dy,
            "coupling": max(abs(r_dx), abs(r_dy))}


def score(rollouts: Path, actions: Path, *, min_delta: float = DEFAULT_MIN_DELTA,
          downscale: int = DEFAULT_DOWNSCALE, limit: int | None = None) -> dict:
    vids = sorted(rollouts.rglob("*.mp4"))[: limit or None]
    if not vids:
        raise SystemExit(f"{rollouts} 下没有 mp4")

    all_est, all_lab, missing = [], [], []
    for v in vids:
        npz = next(actions.rglob(f"{v.stem}.npz"), None)
        if npz is None:
            missing.append(v.stem)
            continue
        mouse = np.load(npz)["mouse"]
        est, lab = _pairs(_decode(v, downscale), mouse,
                          min_delta=min_delta, downscale=downscale)
        if est.size:
            all_est.append(est)
            all_lab.append(lab)
    if not all_est:
        raise SystemExit("没有可用帧对 —— 检查动作 npz 是否与 rollout 同名")
    est, lab = np.concatenate(all_est), np.concatenate(all_lab)

    # 分位移档: p90 以上只作观察项(见模块 docstring 纪律 1)
    mag = np.abs(lab).max(axis=1)
    p90 = float(np.percentile(mag, 90))
    small, large = mag < p90, mag >= p90
    return {
        "rollouts": len(vids),
        "actions_missing": missing,
        "min_delta": min_delta,
        "p90_displacement": p90,
        "all": _coupling(est, lab),
        # 判据档
        "small_delta": _coupling(est[small], lab[small]),
        # 观察项: 估计器在这一档先失效, 不作判据
        "large_delta_observational_only": _coupling(est[large], lab[large]),
        "ground_truth_coupling": GROUND_TRUTH_COUPLING,
    }


def report(r: dict, control: dict | None = None, label: str = "run") -> str:
    def frac(c):
        return c["coupling"] / GROUND_TRUTH_COUPLING
    L = [
        f"# E1a 视角遵循 · {label}", "",
        f"- rollout: {r['rollouts']} 条 · min_delta {r['min_delta']} · "
        f"p90 位移 {r['p90_displacement']:.0f} counts",
        f"- 真值上界(同一估计器在真实录像上): **{GROUND_TRUTH_COUPLING}** —— "
        "生成画面不可能比真实录像更容易被光流读出动作", "",
        "| 档 | 帧对 | r_dx | r_dy | 耦合 | 占真值 | 判据? |",
        "|---|---:|---:|---:|---:|---:|---|",
    ]
    for key, name, is_gate in (("small_delta", f"|Δ| < p90", True),
                               ("large_delta_observational_only", "|Δ| ≥ p90", False),
                               ("all", "全部", False)):
        c = r[key]
        L.append(f"| {name} | {c['pairs']} | {c['r_dx']:.3f} | {c['r_dy']:.3f} | "
                 f"**{c['coupling']:.3f}** | {100 * frac(c):.1f}% | "
                 f"{'✅ 判据' if is_gate else '观察项'} |")
    L += ["", "> `|Δ| ≥ p90` 只作观察项：Lane A 实测未截断信号的 r_dx 反而更低，"
          "大位移上**瓶颈是量具不是标签**，把它计入判据会把估计器的失效读成模型的失败。"]
    if control is None:
        L += ["", "🟥 **没有并列对照。** 一个只会「画面在动」的模型也能拿到非零耦合"
              "（镜头本来就在动）——**没有「不喂动作」对照的 E1a 读数不能用**。"]
    else:
        a, b = r["small_delta"]["coupling"], control["small_delta"]["coupling"]
        L += ["", "## 与不喂动作的对照并列（判据档）", "",
              "| | 耦合 | 占真值 |", "|---|---:|---:|",
              f"| 本轮 | {a:.3f} | {100 * a / GROUND_TRUTH_COUPLING:.1f}% |",
              f"| 不喂动作 | {b:.3f} | {100 * b / GROUND_TRUTH_COUPLING:.1f}% |",
              f"| **增量** | **{a - b:+.3f}** | |", "",
              ("🟩 动作确实带来了信号。" if a - b > 0.05 else
               "🟥 **与不喂动作没有区别** —— 这个数不能读成「学会了动作」。")]
    return "\n".join(L) + "\n"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rollouts", type=Path, required=True)
    ap.add_argument("--actions", type=Path, required=True)
    ap.add_argument("--control", type=Path, default=None,
                    help="不喂动作的 rollout 目录（强烈建议给）")
    ap.add_argument("--label", default="run")
    ap.add_argument("--min-delta", type=float, default=DEFAULT_MIN_DELTA)
    ap.add_argument("--downscale", type=int, default=DEFAULT_DOWNSCALE)
    ap.add_argument("--limit", type=int, default=None)
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--json", type=Path, default=None)
    a = ap.parse_args()
    kw = dict(min_delta=a.min_delta, downscale=a.downscale, limit=a.limit)
    r = score(a.rollouts, a.actions, **kw)
    ctrl = score(a.control, a.actions, **kw) if a.control else None
    if a.json:
        a.json.write_text(json.dumps({"run": r, "control": ctrl}, indent=1) + "\n")
    md = report(r, ctrl, a.label)
    if a.out:
        a.out.write_text(md)
        print(f"→ {a.out}")
    print(md)


if __name__ == "__main__":
    main()
