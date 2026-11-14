"""采集数据就绪自检（接线待用）：不转码，快速验证 converter ready 目录能否被训练线消费。

用法:
    python -m diogenes_player.preflight --src <converter-ready-dir> [--scheme dm1-v2]

对每个 session 检查并汇总报告:
- 发现的 session/chunk/frame 数（深度无关发现，与 ingest 同一套逻辑）
- 治理门：trainable vs non_trainable（真实数据在 grant 前恒为 non_trainable，属预期）
- schema：meta 必需字段、p0.jsonl 行数==meta.frames、含 actions 列
- fps 一致性：每 chunk 必须声明帧率，优先读有理数 video.fps_num/fps_den（59.94≠60，
  整数 fps 仅退化用）；全语料只允许单一 fps（或 --expect-fps 钉死），
  混合帧率=阻断——window/pointer/framepair 全是帧量纲（DataMiner docs/50 P-4/P-8）
- μ-law：抽样重算 bins 与存储是否一致（scheme 判定；不一致=契约漂移，红字告警）
- run_frame_ranges 是否存在（采样不跨 run 的依据）
- 动作覆盖：COD 词表里哪些语义在数据中出现过（support>0），哪些从未出现（提示采集缺口）

退出码非零 = 有阻断问题（schema/契约漂移/发现不到数据），供 CI/脚本 gate。
"""
from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path

from . import contract
from .action_registry import default_cod_vocab
from .ingest import (
    _CHUNK_RE,
    _session_identity,
    audit_physical_inputs,
    discover_sessions,
)
from .physical_vocab import (
    PhysicalInputVocab,
    add_physical_vocab_arguments,
    physical_vocab_from_args,
    validate_physical_vocab_scheme,
)


def preflight(src: Path, *, physical_vocab: PhysicalInputVocab,
              scheme: str = contract.DEFAULT_SCHEME,
              sample_rows: int = 200, expect_fps: float | None = None) -> dict:
    vocab = default_cod_vocab()
    report: dict = {
        "src": str(src), "scheme": scheme,
        "physical_vocab_hash": physical_vocab.content_hash,
        "sessions": 0, "chunks": 0, "frames": 0,
        "trainable_chunks": 0, "non_trainable_chunks": 0,
        "games": {}, "fps": {}, "sessions_without_run_ranges": [],
        "bin_drift": [], "schema_errors": [], "blocking": [],
        "view_scale_frames": 0,
    }
    action_counts: Counter = Counter()

    try:
        validate_physical_vocab_scheme(physical_vocab, scheme)
    except ValueError as exc:
        report["blocking"].append(str(exc))

    session_dirs = discover_sessions(src)
    if not session_dirs:
        report["blocking"].append(f"未发现任何 cNNNN/meta.json（--src={src} 层级是否正确？）")
        return report

    physical_audit = audit_physical_inputs(src, physical_vocab)
    report["physical_input_audit"] = physical_audit
    if physical_audit["blocking"]:
        report["blocking"].append(
            "物理输入契约失败：未知 VK/鼠标键或 mapping 身份/哈希与冻结词表不一致")

    for sdir in session_dirs:
        try:
            game, session = _session_identity(sdir)
        except Exception as e:  # noqa: BLE001
            report["schema_errors"].append(f"{sdir}: {e}")
            continue
        report["sessions"] += 1
        report["games"][game] = report["games"].get(game, 0) + 1

        rr = (sdir / "metadata.json")
        has_rr = False
        if rr.exists():
            try:
                prov = json.loads(rr.read_text(encoding="utf-8")).get(
                    "provenance", {})
                has_rr = bool(prov.get("run_frame_ranges"))
            except json.JSONDecodeError:
                pass
        if not has_rr:
            report["sessions_without_run_ranges"].append(f"{game}/{session}")

        for chunk_dir in sorted(d for d in sdir.glob("c*") if _CHUNK_RE.match(d.name)):
            meta_f = chunk_dir / "meta.json"
            jl = chunk_dir / "p0.jsonl"
            if not meta_f.exists() or not jl.exists():
                report["schema_errors"].append(f"{chunk_dir}: 缺 meta.json 或 p0.jsonl")
                continue
            try:
                meta = json.loads(meta_f.read_text(encoding="utf-8"))
                n_expected = int(meta["video"]["frames"])
            except (json.JSONDecodeError, KeyError) as e:
                report["schema_errors"].append(f"{meta_f}: {e}")
                continue
            report["chunks"] += 1
            if meta.get("is_trainable", False):
                report["trainable_chunks"] += 1
            else:
                report["non_trainable_chunks"] += 1

            # fps 是帧量纲语义的分母,缺失不猜、混合不吞（docs/50 P-4）。
            # 优先读有理数真值 fps_num/fps_den：整数 fps 是近似,59.94(NTSC)
            # 取整后与 60.00 同为 60,恰是这道门要拦的静默混合（docs/50 P-8）。
            video = meta.get("video") or {}
            num, den = video.get("fps_num"), video.get("fps_den")
            if isinstance(num, (int, float)) and isinstance(den, (int, float)) \
                    and num > 0 and den > 0:
                fps_value = num / den
            else:
                fps_raw = video.get("fps", meta.get("fps"))
                fps_value = float(fps_raw) if fps_raw is not None else None
            if fps_value is None:
                report["schema_errors"].append(
                    f"{meta_f}: 缺 video.fps（拒绝静默假定 30；混合 fps 防线）")
            else:
                key = f"{fps_value:g}"
                report["fps"][key] = report["fps"].get(key, 0) + 1

            n = 0
            for i, line in enumerate(jl.open(encoding="utf-8")):
                row = json.loads(line)
                n += 1
                for a in row.get("actions", []):
                    action_counts[a] += 1
                if row.get("view_scale_valid"):
                    report["view_scale_frames"] += 1
                if i < sample_rows:
                    dx, dy = int(row.get("pointerDx", 0)), int(row.get("pointerDy", 0))
                    bx, by = row.get("pointerBins", [None, None])
                    if bx is not None and (contract.encode_bin(dx, scheme) != bx
                                           or contract.encode_bin(dy, scheme) != by):
                        report["bin_drift"].append(
                            f"{chunk_dir.name} f{i}: dx={dx} dy={dy} stored=({bx},{by}) "
                            f"!= {scheme}=({contract.encode_bin(dx, scheme)},"
                            f"{contract.encode_bin(dy, scheme)})")
            report["frames"] += n
            if n != n_expected:
                report["schema_errors"].append(
                    f"{jl}: 行数 {n} != meta.frames {n_expected}")

    covered = sorted(a for a in vocab.actions if action_counts.get(a, 0) > 0)
    missing = sorted(a for a in vocab.actions if action_counts.get(a, 0) == 0)
    report["action_coverage"] = {
        "covered": covered, "missing": missing,
        "coverage_pct": round(100 * len(covered) / max(vocab.n_actions, 1), 1),
        "top": dict(action_counts.most_common(10)),
    }

    fps_values = sorted(report["fps"], key=float)
    if expect_fps is not None:
        wrong = {k: v for k, v in report["fps"].items() if float(k) != float(expect_fps)}
        if wrong:
            report["blocking"].append(
                f"fps 不符 --expect-fps={expect_fps:g}：{wrong}（一次训练只消费单一 fps）")
    elif len(fps_values) > 1:
        report["blocking"].append(
            f"混合 fps 语料 {fps_values}（chunk 数 {report['fps']}）——"
            "过滤数据或用 --expect-fps 钉死单一帧率")

    frames = report["frames"]
    report["view_scale_coverage_pct"] = (
        round(100 * report["view_scale_frames"] / frames, 2) if frames else 0.0)
    # 未标定不是阻断项——现阶段全部语料都还没标定（counts_per_360 待 H-1 实测）。
    # 但必须显式报出来：原始计数是灵敏度相关的，拿它训 camera 头会锁死在一套灵敏度上。
    if frames and report["view_scale_frames"] == 0:
        report.setdefault("warnings", []).append(
            "视角尺度未标定（view_scale_valid 全 False，counts_per_360 尚未实测）："
            "camera 头只能用 μ-law 计数箱训练，结果与本次采集的鼠标灵敏度绑定、跨游戏不可比。"
            "见 DataMiner/docs/45。")

    if report["bin_drift"]:
        report["blocking"].append(
            f"μ-law 契约漂移 {len(report['bin_drift'])} 处（scheme={scheme} 与存储不符）")
    if report["schema_errors"]:
        report["blocking"].append(f"schema 错误 {len(report['schema_errors'])} 处")
    return report


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--src", type=Path, required=True, help="converter ready 目录（任意层级）")
    ap.add_argument("--scheme", default=contract.DEFAULT_SCHEME, choices=["dm1-v2", "legacy"])
    ap.add_argument("--sample-rows", type=int, default=200, help="每 chunk 抽样对账的行数")
    ap.add_argument("--expect-fps", type=float, default=None,
                    help="钉死语料帧率；缺省要求全语料 fps 一致")
    add_physical_vocab_arguments(ap)
    args = ap.parse_args()
    physical_vocab = physical_vocab_from_args(args)
    report = preflight(args.src, physical_vocab=physical_vocab,
                       scheme=args.scheme, sample_rows=args.sample_rows,
                       expect_fps=args.expect_fps)
    print(json.dumps(report, indent=2, ensure_ascii=False))
    if report["blocking"]:
        print("\n就绪自检: 阻断 —", "; ".join(report["blocking"]), file=sys.stderr)
        sys.exit(1)
    for w in report.get("warnings", []):
        print(f"\n⚠️  {w}")
    print(f"\n就绪自检: 通过（{report['sessions']} sessions / {report['chunks']} chunks / "
          f"{report['frames']} frames；trainable={report['trainable_chunks']}，"
          f"动作覆盖 {report['action_coverage']['coverage_pct']}%，"
          f"视角标定 {report['view_scale_coverage_pct']}%）")


if __name__ == "__main__":
    main()
