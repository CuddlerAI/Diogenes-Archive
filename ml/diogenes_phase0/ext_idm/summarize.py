"""Combine several score.py reports into the markdown tables research/30 quotes.

    python -m diogenes_phase0.ext_idm.summarize report_*.json > tables.md

Exists so the document's numbers are generated from the artifacts rather than
retyped. Every row carries the split, the resolution condition and the window
count, because the one thing a reader must not be able to do is compare two
rows that were scored on different frames.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def _fmt(v, nd=4):
    if v is None:
        return "—"
    return f"{v:.{nd}f}"


def _ci(v):
    return "—" if not v else f"[{v[0]:.3f}, {v[1]:.3f}]"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("reports", nargs="+", type=Path)
    ap.add_argument("--seed", default="gt", choices=["gt", "none"])
    args = ap.parse_args()

    reports = [json.loads(p.read_text()) for p in args.reports]

    print("### 主表\n")
    print("| 评测集 | 窗口数 | 系统 | key_macro_f1 | CI95 | key_exact |")
    print("|---|---|---|---|---|---|")
    for r in reports:
        shift = r["primary_shift_ms"]
        primary = f"gidm1b.seed={args.seed}.shift={shift:g}ms"
        order = [(primary, f"Generalist-IDM-1B (seed={args.seed}, {shift:g}ms)"),
                 ("ours", "我们的头 4,750 步"),
                 ("trivial.always_on", "平凡基线·恒按下"),
                 ("trivial.persist_seed", "平凡基线·把 seed 一直按住"),
                 ("trivial.all_off", "平凡基线·恒不按"),
                 ("trivial.majority_set", "平凡基线·最频繁键集")]
        for i, (key, label) in enumerate(order):
            s = r["systems"].get(key)
            if not s:
                continue
            head = f"{r['label']} | {r['n_windows']}" if i == 0 else " | "
            print(f"| {head} | {label} | {_fmt(s.get('key_macro_f1'))} | "
                  f"{_ci(s.get('key_macro_f1_ci95'))} | {_fmt(s.get('key_exact'))} |")

    print("\n### 配对差值（同一批窗口上自举，1000 次）\n")
    print("| 评测集 | 对比 | Δmacro_f1 | CI95 | 排除 0？ |")
    print("|---|---|---|---|---|")
    for r in reports:
        for name, c in (r.get("contrasts") or {}).items():
            if not c:
                continue
            print(f"| {r['label']} | {name} | "
                  f"{c['delta_key_macro_f1']:+.4f} | "
                  f"{_ci(c['delta_key_macro_f1_ci95'])} | "
                  f"{'是' if c['macro_f1_ci_excludes_zero'] else '否'} |")

    print("\n### 时间位移扫描（macro_f1）\n")
    shifts = sorted({float(k.split("shift=")[1].rstrip("ms"))
                     for r in reports for k in r["systems"]
                     if k.startswith("gidm1b.")})
    print("| 评测集 | seed | " + " | ".join(f"{s:g}ms" for s in shifts) + " |")
    print("|---" * (len(shifts) + 2) + "|")
    for r in reports:
        for seed in ("gt", "none"):
            cells = []
            for s in shifts:
                v = r["systems"].get(f"gidm1b.seed={seed}.shift={s:g}ms")
                cells.append(_fmt(v.get("key_macro_f1")) if v else "—")
            print(f"| {r['label']} | {seed} | " + " | ".join(cells) + " |")

    print("\n### 块内位置（同一 key 集下的 macro_f1）——seed 的好处会不会随距离衰减\n")
    print("| 评测集 | 系统 | 前 1/3 | 中 1/3 | 后 1/3 |")
    print("|---|---|---|---|---|")
    for r in reports:
        sh = r["primary_shift_ms"]
        rows = [(f"gidm1b.seed=gt.shift={sh:g}ms", "G-IDM seed=gt"),
                (f"gidm1b.seed=none.shift={sh:g}ms", "G-IDM seed=none"),
                ("ours", "我们的头"),
                ("trivial.always_on", "平凡基线·恒按下"),
                ("trivial.persist_seed", "平凡基线·把 seed 一直按住")]
        for key, label in rows:
            v = r["systems"].get(key)
            if not v or not v.get("by_chunk_third"):
                continue
            print(f"| {r['label']} | {label} | "
                  + " | ".join(_fmt(x) for x in v["by_chunk_third"]) + " |")

    print("\n### 指针相关（仅信息项，D-4 后不进判据）\n")
    print("| 评测集 | 系统 | pearson_x | pearson_y |")
    print("|---|---|---|---|")
    for r in reports:
        v = r["systems"].get(
            f"gidm1b.seed={args.seed}.shift={r['primary_shift_ms']:g}ms")
        if v and v.get("pointer_pearson_r"):
            p = v["pointer_pearson_r"]
            print(f"| {r['label']} | Generalist-IDM-1B | "
                  f"{_fmt(p.get('x'), 3)} | {_fmt(p.get('y'), 3)} |")

    print("\n### 词表投影丢掉了 G-IDM 多少输出\n")
    print("| 评测集 | 预测键盘事件 | 落在 12 键词表外 |")
    print("|---|---|---|")
    for r in reports:
        v = r["systems"].get(
            f"gidm1b.seed={args.seed}.shift={r['primary_shift_ms']:g}ms")
        if v and v.get("pred_keyboard_events"):
            print(f"| {r['label']} | {v['pred_keyboard_events']} | "
                  f"{v['pred_keyboard_out_of_vocab_frac']:.1%} |")

    print("\n### 自检与规模\n")
    for r in reports:
        sc = r.get("selfcheck") or {}
        print(f"* `{r['label']}`：{r['n_chunks']} chunk / {r['n_windows']} 窗口，"
              f"会话 {r['sessions']}，"
              f"与 `evaluate.run_eval` 一致="
              f"{sc.get('matches_stock_run_eval')}（差 "
              f"{sc.get('delta_key_macro_f1', float('nan')):.2e}），"
              f"缺预测而丢弃 {r.get('chunks_dropped_no_prediction', 0)} chunk")


if __name__ == "__main__":
    main()
