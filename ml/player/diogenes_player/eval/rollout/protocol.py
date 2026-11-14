"""W4 评估协议: 固化 episode 定义与 K/D 记账 (research/04 §5.2)。

- aim_botz: 60s × N 组 (CS1 门: 命中数 ≥70% 新手均值)
- deathmatch: 30min × N 局取中位 (CS2 门: K/D ≥1.5 Easy / ≥1.0 Medium)
协议只负责计时窗口 + GSI 事件差分 + CSV; 起服务器/换图由 runner/操作手册负责。
"""
from __future__ import annotations

import csv
import time
from dataclasses import asdict, dataclass
from pathlib import Path

from .gsi import GsiState
from .loop import RolloutLoop


@dataclass
class EpisodeResult:
    protocol: str
    episode: int
    duration_s: float
    kills: int
    deaths: int
    kd: float
    ticks: int
    total_ms_p99: float


def run_episodes(loop: RolloutLoop, gsi: GsiState, *, protocol: str,
                 episodes: int, duration_s: float,
                 on_episode_start=None) -> list[EpisodeResult]:
    results = []
    for ep in range(episodes):
        if on_episode_start:
            on_episode_start(ep)
        loop.policy.reset()
        n_ticks_before = len(loop.telemetry.ticks)
        t0 = time.monotonic()
        loop.run(duration_s=duration_s)
        kills, deaths = gsi.counts_since(t0)
        summ = loop.telemetry.summary()
        results.append(EpisodeResult(
            protocol=protocol, episode=ep,
            duration_s=round(time.monotonic() - t0, 1),
            kills=kills, deaths=deaths,
            kd=round(kills / max(deaths, 1), 3),
            ticks=len(loop.telemetry.ticks) - n_ticks_before,
            total_ms_p99=summ.get("total_ms", {}).get("p99", 0.0),
        ))
    return results


def median_kd(results: list[EpisodeResult]) -> float:
    kds = sorted(r.kd for r in results)
    n = len(kds)
    return kds[n // 2] if n % 2 else (kds[n // 2 - 1] + kds[n // 2]) / 2


def to_csv(results: list[EpisodeResult], path: str | Path) -> None:
    with Path(path).open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(asdict(results[0]).keys()))
        w.writeheader()
        for r in results:
            w.writerow(asdict(r))
