#!/usr/bin/env python3
"""**所有验收阈值的唯一出处**，每一条都带口径来源。

## 为什么要有这个文件

2026-08-12 的教训（docs/121 §13.2）：卡死整批 Loop-有位移 数据的三个闸——单方向 ≤2s、
甩头 ≤90°/s、闭环 0.60m——**没有一个是客户硬要求**。单方向是客户原文自己标注的
「非硬性」；甩头是我方 v1 建议档；闭环 0.60m 是 trajgen **引擎线生成侧**的自检阈值，
而 docs/70 §1 我方自陈「真人手玩达不到首尾帧非常相近的精度」。

阈值散在各脚本里、又不写出处，下游就会把我方的建议档当成客户要求去打回数据。
本单已经据此误判过一轮，也让采集员按错的优先级返工过。

**所以：任何新增阈值必须落在本文件，并声明 `source`。** 判定侧只允许把
`CUSTOMER_HARD` 判成「不通过」，其余一律降级为「需人工确认」。

## 三档口径

| source | 含义 | 判定 |
|---|---|---|
| `CUSTOMER_HARD` | docs/121 §1.3/§1.4 明列的红线与 json/video 规格 | 可判「不通过」 |
| `CUSTOMER_SOFT` | 客户原文自己标了「非硬性」 | 只能「需人工确认」 |
| `OURS` | 我方 v1 建议档 / 引擎线自检阈值 | 只能「需人工确认」 |
"""
from __future__ import annotations

from dataclasses import dataclass

CUSTOMER_HARD = "客户硬性"
CUSTOMER_SOFT = "客户非硬性"
OURS = "我方档"


@dataclass(frozen=True)
class Threshold:
    value: float
    source: str
    origin: str          # 出处：文档条目或代码位置
    note: str = ""

    def __float__(self):
        return float(self.value)


T = {
    # ---- 客户硬性 ---------------------------------------------------------
    "duration_frames_60s": Threshold(
        1800, CUSTOMER_HARD, "docs/121 §1.3 / 客户脚本硬编码",
        "60s 档必须恰好 1800 帧，1799/1801 都不过"),
    "duration_frames_180s": Threshold(
        5400, CUSTOMER_HARD, "docs/121 §1.3", "3min 档；客户脚本当前版本尚不支持"),
    "video_width": Threshold(1920, CUSTOMER_HARD, "docs/121 §1.3", ""),
    "video_height": Threshold(1080, CUSTOMER_HARD, "docs/121 §1.3", ""),
    "fps": Threshold(30, CUSTOMER_HARD, "docs/121 §1.3/§1.4",
                     "非 30fps 或与 json 帧率不统一 = **整批**打回不结算"),
    "static_frac_max": Threshold(
        0.10, CUSTOMER_HARD, "docs/121 §1.4 定性打回项",
        "「>10% 时长原地静止」是客户明列的打回项"),
    "hfov_targets_deg": Threshold(
        75.0, CUSTOMER_HARD, "业务方 2026-08-11 反馈①（docs/121 §9）",
        "锁水平 FOV 75 或 60；另见 hfov_tol_deg"),
    "hfov_tol_deg": Threshold(2.0, CUSTOMER_HARD, "同上", "标定误差带"),
    "dynamics_present_frac": Threshold(
        0.60, CUSTOMER_HARD, "客户 60-30 口径",
        "60% 镜头要有动态物体；**目前只能抽帧目检**，机检未验证（docs/121 §13.5）"),
    "dynamics_moving_frac": Threshold(
        0.30, CUSTOMER_HARD, "客户 60-30 口径",
        "30% 镜头动态物体在移动；同上，机检未验证"),

    # ---- 客户非硬性（原文自己标的） ---------------------------------------
    "one_direction_max_s": Threshold(
        2.0, CUSTOMER_SOFT, "docs/121 §1.2",
        "原文：「**扰动（非硬性）**：单方向移动/摇镜不得持续 2s 以上」"),

    # ---- 我方档（v1 建议 / 引擎线自检） ------------------------------------
    "loop_pos_tol_m": Threshold(
        0.60, OURS, "trajgen/sfm.py:448（引擎线**生成侧**自检）",
        "客户原文只有「首尾帧**尽量**保持一致…**尽量**非常相近」；"
        "docs/70 §1 自陈真人手玩达不到该精度 —— 用于人工采集时须说明这是我方档"),
    "loop_rot_tol_deg": Threshold(8.0, OURS, "trajgen/sfm.py:449", "同上"),
    "rot_peak_max_dps": Threshold(
        90.0, OURS, "handheld_feel_check v1 建议档",
        "客户侧只有定性项「快速切镜/抖动」；#717 人工采集档位定形后以彼为准"),
    "rot_median_lo_dps": Threshold(5.0, OURS, "handheld_feel_check v1", "手持感下限"),
    "rot_median_hi_dps": Threshold(25.0, OURS, "handheld_feel_check v1", "手持感上限"),
    "rot_locked_frac_max": Threshold(0.10, OURS, "handheld_feel_check v1", "朝向锁定帧占比"),
    "idle_run_max_s": Threshold(3.0, OURS, "handheld_feel_check v1", "最长连续静止"),
    "ptr_moving_frac_min": Threshold(
        0.50, OURS, "SOP v2 推导", "「右手不离鼠标」的量化下限"),
    "dup_frac_max": Threshold(
        0.01, OURS, "docs/121 §11.3 引入",
        "补帧 = 游戏没跑满 30fps。容器仍是 CFR30 故客户脚本查不出，"
        "但补帧之间基线为零、对 SFM 直接有害，且逼近 §1.4「video 非 30fps 整批打回」"),

    # ---- 判据内部参数（非验收口径，改动只影响检测灵敏度） ------------------
    "idle_pos_ms": Threshold(0.05, OURS, "handheld_feel_check", "静止判定的平移速度上限"),
    "idle_rot_dps": Threshold(2.0, OURS, "handheld_feel_check", "静止判定的旋转速率上限"),
    "fov_zoom_step_tol": Threshold(0.03, OURS, "docs/121 §10.2", "逐帧缩放台阶阈值"),
    "fov_min_pair_rot_deg": Threshold(2.0, OURS, "docs/121 §12.3", "标定帧对最小旋转量"),
}


def val(key) -> float:
    return float(T[key].value)


def src(key) -> str:
    return T[key].source


def is_hard(key) -> bool:
    return T[key].source == CUSTOMER_HARD


def describe(key) -> str:
    t = T[key]
    return f"{t.value}（{t.source}；{t.origin}）"


def table_md() -> str:
    lines = ["| 阈值 | 值 | 口径出处 | 来源 | 说明 |", "|---|---|---|---|---|"]
    for k, t in T.items():
        lines.append(f"| `{k}` | {t.value} | **{t.source}** | {t.origin} | {t.note} |")
    return "\n".join(lines)


if __name__ == "__main__":
    print(table_md())
