"""批目录汇总 + 模拟需求方抽检协议（docs/71 §9 G-18）。

协议：每场景抽 2 条做 json 全检、抽 5% 做 video 检；
打回线：2 条 json 双不过，或 video 通过率 <90% → 整包打回。
"""

import math
import random
from pathlib import Path
from typing import List, Optional

from . import checks_video as cv
from .clip_checker import check_clip
from .config import QCConfig
from .loaders import discover, discover_batch_clips
from .report import FAIL, PASS, SKIP, UNVERIFIED, ClipReport, render_clip_text


class BatchReport:
    def __init__(self, batch: str, mode: str):
        self.batch = batch
        self.mode = mode
        self.clips: List[ClipReport] = []
        self.protocol: dict = {}

    @property
    def verdict(self) -> str:
        if self.protocol.get("verdict") == "REJECT":
            return FAIL
        if any(c.verdict == FAIL for c in self.clips):
            return FAIL
        if any(c.verdict == UNVERIFIED for c in self.clips):
            return UNVERIFIED
        return PASS

    def to_dict(self) -> dict:
        return {
            "batch": self.batch,
            "mode": self.mode,
            "verdict": self.verdict,
            "summary": self.summary(),
            "protocol": self.protocol,
            "clips": [c.to_dict() for c in self.clips],
        }

    def summary(self) -> dict:
        rule_fail: dict = {}
        for c in self.clips:
            for r in c.failures():
                rule_fail[r.rule] = rule_fail.get(r.rule, 0) + 1
        return {
            "clips_total": len(self.clips),
            "clips_pass": sum(1 for c in self.clips if c.verdict == PASS),
            "clips_fail": sum(1 for c in self.clips if c.verdict == FAIL),
            "clips_unverified": sum(1 for c in self.clips if c.verdict == UNVERIFIED),
            "rule_fail_histogram": dict(sorted(rule_fail.items(),
                                               key=lambda kv: -kv[1])),
        }


def check_batch(root, mode: str, cfg: Optional[QCConfig] = None,
                seed: int = 20260803) -> BatchReport:
    cfg = cfg or QCConfig()
    root = Path(root)
    br = BatchReport(batch=str(root), mode=mode)
    clip_dirs = discover_batch_clips(root)
    if not clip_dirs:
        br.protocol = {"verdict": "REJECT", "reason": f"批目录内未发现任何 clip：{root}"}
        return br

    # 全量 json 面自检（video 面只对协议抽样跑，避免逐条 ffprobe）
    for d in clip_dirs:
        br.clips.append(check_clip(d, mode, cfg, include_video=False))

    rng = random.Random(seed)
    names = [str(d) for d in clip_dirs]
    idx = list(range(len(clip_dirs)))

    json_sample = sorted(rng.sample(idx, min(cfg.sample_json_n, len(idx))))
    json_fails = [names[i] for i in json_sample
                  if br.clips[i].json_verdict() == FAIL]
    json_reject = 0 < len(json_sample) == len(json_fails)

    video_n = max(1, math.ceil(cfg.sample_video_frac * len(idx)))
    video_sample = sorted(rng.sample(idx, min(video_n, len(idx))))
    video_pass = video_fail = video_skip = video_unver = 0
    for i in video_sample:
        files = discover(clip_dirs[i], mode)
        results = cv.check_videos(files, mode, cfg)
        br.clips[i].extend(results)
        vv = br.clips[i].video_verdict()
        if vv == PASS:
            video_pass += 1
        elif vv == FAIL:
            video_fail += 1
        elif vv == UNVERIFIED:
            video_unver += 1
        else:
            video_skip += 1
    checked = video_pass + video_fail
    video_rate = (video_pass / checked) if checked else None
    video_reject = video_rate is not None and video_rate < cfg.video_pass_line

    verdict = "REJECT" if (json_reject or video_reject) else "ACCEPT"
    br.protocol = {
        "seed": seed,
        "json_sampled": [names[i] for i in json_sample],
        "json_sampled_fail": json_fails,
        "json_gate": "REJECT" if json_reject else "PASS",
        "video_sampled": [names[i] for i in video_sample],
        "video_checked": checked,
        "video_skipped": video_skip,
        "video_unverified": video_unver,
        "video_pass_rate": video_rate,
        "video_gate": ("REJECT" if video_reject
                       else ("PASS" if checked
                             else ("UNVERIFIED（有 mp4 但 ffprobe/ffmpeg 均不可用）" if video_unver
                                   else "N/A（无视频文件）"))),
        "verdict": verdict,
    }
    return br


def render_batch_text(br: BatchReport, verbose_fail: bool = True) -> str:
    lines = [f"== engine-data-qc · batch {br.batch} · mode={br.mode} · "
             f"{len(br.clips)} clips =="]
    s = br.summary()
    for c in br.clips:
        mark = c.verdict
        n_fail = len(c.failures())
        lines.append(f"[{mark}] {c.clip}"
                     + (f"（{n_fail} 条规则 FAIL）" if n_fail else ""))
        if verbose_fail and n_fail:
            for r in c.failures()[:10]:
                lines.append(f"    - {r.rule} {r.target}: {r.evidence}")
    lines.append(f"-- 汇总：{s['clips_pass']}/{s['clips_total']} 条通过；"
                 f"规则失败分布 {s['rule_fail_histogram'] or '{}'}")
    p = br.protocol
    lines.append("-- 模拟需求方抽检协议（抽 2 条 json 全检 + 5% video 检、90% 线）：")
    lines.append(f"   json 抽检：{p.get('json_sampled')} → 失败 {p.get('json_sampled_fail')} "
                 f"→ 闸门 {p.get('json_gate')}")
    rate = p.get("video_pass_rate")
    lines.append(f"   video 抽检：{p.get('video_sampled')} → 有效 {p.get('video_checked')} 条、"
                 f"SKIP {p.get('video_skipped')} 条、通过率 "
                 f"{f'{rate*100:.0f}%' if rate is not None else 'N/A'} → 闸门 {p.get('video_gate')}")
    lines.append(f"   协议判定：{p.get('verdict')}"
                 + (f"（{p['reason']}）" if p.get("reason") else ""))
    lines.append(f"-- 批判定：{br.verdict}")
    return "\n".join(lines)
