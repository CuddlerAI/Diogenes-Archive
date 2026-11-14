"""W4 延迟遥测: 每 tick 三段打点 (capture/infer/inject), research/04 §7 上线门的测量工具。"""
from __future__ import annotations

import csv
from dataclasses import dataclass
from pathlib import Path

import numpy as np


@dataclass
class Tick:
    t_start: float
    t_capture: float   # 采集完成
    t_infer: float     # 推理完成
    t_inject: float    # 注入完成


class Telemetry:
    def __init__(self) -> None:
        self.ticks: list[Tick] = []

    def add(self, tick: Tick) -> None:
        self.ticks.append(tick)

    def summary(self) -> dict:
        if not self.ticks:
            return {"n": 0}
        a = np.array([[t.t_start, t.t_capture, t.t_infer, t.t_inject]
                      for t in self.ticks])
        stages = {
            "capture_ms": (a[:, 1] - a[:, 0]) * 1e3,
            "infer_ms": (a[:, 2] - a[:, 1]) * 1e3,
            "inject_ms": (a[:, 3] - a[:, 2]) * 1e3,
            "total_ms": (a[:, 3] - a[:, 0]) * 1e3,
        }
        out: dict = {"n": len(self.ticks)}
        if len(a) > 1:
            stages["period_ms"] = np.diff(a[:, 0]) * 1e3
        for name, v in stages.items():
            out[name] = {"p50": round(float(np.percentile(v, 50)), 2),
                         "p90": round(float(np.percentile(v, 90)), 2),
                         "p99": round(float(np.percentile(v, 99)), 2)}
        return out

    def to_csv(self, path: str | Path) -> None:
        with Path(path).open("w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["t_start", "capture_ms", "infer_ms", "inject_ms", "total_ms"])
            for t in self.ticks:
                w.writerow([f"{t.t_start:.6f}",
                            f"{(t.t_capture - t.t_start) * 1e3:.3f}",
                            f"{(t.t_infer - t.t_capture) * 1e3:.3f}",
                            f"{(t.t_inject - t.t_infer) * 1e3:.3f}",
                            f"{(t.t_inject - t.t_start) * 1e3:.3f}"])
