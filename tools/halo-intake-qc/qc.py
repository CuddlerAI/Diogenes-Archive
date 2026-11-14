#!/usr/bin/env python3
"""Halo 生产摄取的分布 QC——对最近 N 天的 clip 抽样跑 116 号需求书 §8.3 的硬阈值。

动机（research/44 §3.3、docs/92 §4、docs/111 §5）：这批语料曾带着四个会静默污染训练的
分布缺陷入库（combat.aim toggle 签名、空帧超标、纯 W 超标、键覆盖不足），而现有服务端
质量闸只验工程健康位，不验分布。本脚本把 116 §8.3 的验收线变成对自采数据同样生效的
机器检查，供人工巡检或 cron 使用。

判据实现在 `ml/diogenes_wm/halo_qc.py`——本脚本只是它的 IO 外壳（列对象 / 下载 /
解压 / 打印）。训练侧的 `halo_adapter.py` 用同一份库选片，两边"过闸"的定义因此
是同一个。判据、阈值、两档的差别全部见该模块的 docstring。

用法（凭据配置同 tools/r2-attested-reclass/README）：
  qc.py --days 3                  # 列最近 3 天对象，抽样下载 frames 并出报告
  qc.py --listing recent.txt      # 复用已有 `rclone lsf --format tp` 清单（分号分隔）
  qc.py --days 3 --max-clips 300  # 控制抽样规模（默认 240）
  qc.py --days 3 --report qc.json # 额外落一份逐 session 的 JSON 报告（供适配器选片）
  qc.py --days 3 --tier floor     # 换"行为地板"档判定（默认 strict = 116 §8.3 全 10 项）
退出码：0 = 全部通过；1 = 任一硬阈值失败或出现 toggle-zoom 签名。
"""

import argparse
import collections
import json
import os
import random
import re
import subprocess
import sys
import tempfile
from pathlib import Path

# 判据库住在 ml/ 包里；本脚本以裸 python3 运行，加一条 path 而不是要求装包。
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "ml"))
from diogenes_wm.halo_qc import (  # noqa: E402
    dump_report,
    evaluate_batch,
    evaluate_clip,
    evaluate_session,
)

#: 两条**源**车道的 sessions 根。摄取面 2026-08-29 从 v1 切到 v2（issue #855）,
#: 只列 v1 意味着切换后进来的数据抽检不到（docs/167 §3.2）。清单里存桶内完整 key,
#: session 从路径里用正则取, 不再靠 split("/")[0]。
PREFIXES = (
    "raw/v1/non-trainable/dataminer-api-production/sessions",
    "raw/v2/dataminer-api-production/sessions",
)
SESSION_RE = re.compile(r"/sessions/([^/]+)/")
CLIP_SECONDS = 10.0          # segment_seconds（recording profile v3）
FPS = 60.0                   # 该 profile 恒 60；QC 以行数/FPS 折算时长

RCLONE = ["rclone", "--fast-list", "--contimeout", "15s", "--timeout", "300s",
          "--retries", "3", "--low-level-retries", "10"]


def die(msg: str) -> None:
    print(f"FATAL: {msg}", file=sys.stderr)
    sys.exit(2)


def bucket() -> str:
    b = os.environ.get("R2_TRAINING_BUCKET", "diogenes-model-training")
    if not os.environ.get("RCLONE_CONFIG_R2_ACCESS_KEY_ID"):
        die("缺少 RCLONE_CONFIG_R2_* 环境变量（见 tools/r2-attested-reclass/README）")
    return b


def session_of(path: str) -> str:
    m = SESSION_RE.search(path)
    return m.group(1) if m else "_unknown_session"


def list_recent(days: int) -> list[str]:
    lines: list[str] = []
    for prefix in PREFIXES:
        out = subprocess.run(
            [RCLONE[0], "lsf", "-R", "--files-only", "--max-age", f"{days * 24}h",
             "--use-server-modtime", "--format", "tp",
             f"r2:{bucket()}/{prefix}", *RCLONE[1:]],
            capture_output=True, text=True)
        if out.returncode != 0:
            die(f"rclone lsf {prefix} 失败：{out.stderr.strip()[-500:]}")
        for ln in out.stdout.splitlines():
            if ";" in ln:
                ts, rel = ln.split(";", 1)
                lines.append(f"{ts};{prefix}/{rel}")   # 桶内完整 key
    return lines


def decompress(path: Path) -> bytes:
    raw = path.read_bytes()
    try:
        from compression import zstd  # Python ≥3.14
        return zstd.decompress(raw)
    except ImportError:
        pass
    try:
        import zstandard  # ml/ 的 data extra 已依赖它
    except ImportError:
        # 最后退到 zstd CLI（历史路径）
        return subprocess.run(["zstd", "-d", "-c", str(path)],
                              capture_output=True, check=True).stdout
    # ⚠ 必须流式读：这些帧头不带 content size，直接 decompress() 会报错
    import io
    return zstandard.ZstdDecompressor().stream_reader(io.BytesIO(raw)).read()


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--days", type=int, default=3)
    ap.add_argument("--listing", help="已有 rclone lsf --format tp 清单文件")
    ap.add_argument("--max-clips", type=int, default=240)
    ap.add_argument("--seed", type=int, default=44)
    ap.add_argument("--tier", choices=("strict", "floor"), default="strict",
                    help="判定档；strict = 116 §8.3 全 10 项（默认，历史行为），"
                         "floor = doc 50 行为地板（关掉动作熵与 aim 三项）")
    ap.add_argument("--report", help="额外写一份逐 session 的 JSON 报告，"
                                     "供 halo_adapter.py --qc-report 消费")
    args = ap.parse_args()

    lines = (Path(args.listing).read_text().splitlines() if args.listing
             else list_recent(args.days))
    frames_paths = []
    session_clipcount: collections.Counter[str] = collections.Counter()
    for ln in lines:
        p = ln.split(";", 1)[1] if ";" in ln else ln
        if "/frames-" in p and p.endswith(".jsonl.zst"):
            frames_paths.append(p)
            session_clipcount[session_of(p)] += 1
    if not frames_paths:
        die("清单里没有 frames-*.jsonl.zst——时间窗内没有新数据？")

    print(f"时间窗内：{len(session_clipcount)} session / {len(frames_paths)} clip "
          f"≈ {len(frames_paths) * CLIP_SECONDS / 3600:.1f} h")
    short = [s for s, n in session_clipcount.items() if n * CLIP_SECONDS < 600]
    print(f"会话时长 <600s 的比例：{len(short)}/{len(session_clipcount)} "
          f"（116 §8.1 要求为 0；时间窗截断的首尾 session 可人工豁免）\n")

    # 分层抽样：每 session 均匀取，总量封顶 max-clips
    rng = random.Random(args.seed)
    by_session: dict[str, list[str]] = collections.defaultdict(list)
    for p in frames_paths:
        by_session[session_of(p)].append(p)
    quota = max(1, args.max_clips // len(by_session))
    sample = [p for ps in by_session.values()
              for p in rng.sample(sorted(ps), min(quota, len(ps)))]
    print(f"抽样 {len(sample)} clip（每 session ≤{quota}），下载中…", flush=True)

    per_session: dict[str, list] = collections.defaultdict(list)
    with tempfile.TemporaryDirectory() as td:
        listfile = Path(td) / "files.txt"
        listfile.write_text("\n".join(sample))
        # 清单已是桶内完整 key（含车道前缀）, 所以从桶根拉。
        r = subprocess.run([RCLONE[0], "copy", f"r2:{bucket()}", td,
                            "--files-from", str(listfile), "--transfers", "16",
                            "--no-traverse", *RCLONE[1:]],
                           capture_output=True, text=True)
        if r.returncode != 0:
            die(f"下载失败：{r.stderr.strip()[-500:]}")
        for rel in sample:
            sess = session_of(rel)
            rows = [json.loads(ln)
                    for ln in decompress(Path(td) / rel).decode().splitlines()]
            per_session[sess].append(evaluate_clip(rows))

    batch = evaluate_batch(per_session, tier=args.tier, fps=FPS,
                           label=f"halo-intake-{args.days}d")
    print(f"\n== 分布报告（{batch.frames:,} 帧 / {batch.n_sessions} session）==")
    fails = 0
    for c in batch.checks:
        if not c.enabled:
            continue
        fails += not c.passed
        print(f"  {'✅' if c.passed else '❌'} {c.label:<38} {c.reading:>12}   ({c.source})")
    if batch.missing_channels:
        worst = sorted(batch.missing_channels.items(), key=lambda kv: -len(kv[1]))[:5]
        for s, miss in worst:
            print(f"       - {s[:8]}… 缺 {', '.join(miss)}")
    print(f"\n结论：{'全部通过 ✅' if not fails else f'{fails} 项失败 ❌——本时间窗数据按 116 §9.2 批级降权，并回采集侧整改'}")

    if args.report:
        per = [evaluate_session(clips, session_id=sid, tier=args.tier, fps=FPS)
               for sid, clips in sorted(per_session.items())]
        totals = dump_report(args.report, per, tier=args.tier, fps=FPS)["totals"]
        print(f"\n逐 session 报告 → {args.report}："
              f"{totals['passed_sessions']}/{totals['sessions']} 过闸，"
              f"{totals['passed_hours']:.3f} h / {totals['hours']:.3f} h"
              f"（抽样口径，非全量；见 halo_qc 模块 docstring 的抽样敏感性说明）")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
