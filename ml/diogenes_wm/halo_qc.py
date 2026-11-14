"""Halo 语料的 QC 选片门 —— 116 §8.3 行为判据的**纯函数库**.

这个模块存在的理由, 是 doc 50 §4.1 的那条修正: 具结车道治理层承载 29.42 h,
但按项目自己的 QC 闸真正可用的只有一小部分。这个判据此前只活在
`tools/halo-intake-qc/qc.py` 这个**运维巡检脚本**里 —— 它对 R2 抽样、打印一张
人读的表、按退出码告警。训练侧没有任何东西能调用它, 于是"哪些 session 过闸"
这个问题没有机器答案, 适配器只能全量装配或者全量不装配。

本模块把判据从脚本里抽出来, 变成:

    frames.jsonl 的行 → ClipStats → SessionQC → JSON 报告 → 适配器的选片依据

`tools/halo-intake-qc/qc.py` 重构成本模块的一个 IO 外壳 (下载 + 打印), 判据只有
这一份实现; `diogenes_wm.halo_adapter` 用 `--qc-report` 读同一份报告决定装配哪些
session。**两边看到的"过闸"是同一个定义**, 这是把它做成库而不是复制一份的全部意义。

## 判据出处

九项行为判据来自 **116 号需求书 §8.3**(对外供应商的批验收线), 第十项 toggle-zoom
签名来自 **DataMiner docs/111 §5**(92/111 号审计抓到的坏标签形态: `combat.aim` 的
帧占比塌到 `combat.primary_attack` 的 1% 量级, 说明 aim 被记成了 toggle 而不是 hold)。
自采数据没有理由豁免对外供应商的同一条线 —— 这是 116 §8.3 被搬到这里的前提。

## 两档: `floor` 与 `strict`

**`strict`** = 全 10 项, 就是 116 §8.3 + docs/111 §5 的原样。

**`floor`**(doc 50 采用的"行为地板"口径) = `strict` **减去四项 aim/熵相关判据**:

| 判据 id | strict | floor | 为什么 floor 关掉它 |
|---|---|---|---|
| `action_entropy` (≥0.75) | ✅ | ❌ | 熵是对**观测到的全部语义动作**的归一化频率熵, 分母随词表大小变化, 且被 `combat.aim` 的坏标签直接拉低。它衡量的是"动作分布是否丰富", 不是"这段数据能不能用" |
| `aim_share` (`combat.aim` ≥1%) | ✅ | ❌ | 见下方 ⚠ D5: 该数在五个来源有五个值 |
| `toggle_zoom` (aim ≥ 0.2×attack) | ✅ | ❌ | 同上, 是 aim 占比的导出量, 与 `aim_share` 同生共死 |
| `channel_completeness` | 六通道 | **五通道** | floor 不是"关掉通道检查", 而是把 `combat.aim` 从必备通道里摘掉 —— 与 doc 50 §3「`combat.aim` 必须 mask(六通道降五)」一致 |

其余六项(`empty_frames` / `pointer_nonzero` / `pure_w` / `key_coverage` /
`duplicate` / `attack_share`)两档完全相同, 阈值也相同。**两档只差"哪些判据参与
总判定", 不差任何一个阈值** —— 关掉的判据仍然照常计算并写进报告(`enabled=false`),
只是不参与 `passed`。这样一份报告可以事后换档重判而不必重跑。

⚠ **`combat.aim` 占比对抽样极度敏感 —— 这是 doc 50 §7 D5 的记载.**
同一批语料的 `combat.aim` 帧占比在五个来源有五个值: 0.05–0.08%(doc 44)、
0.1%(docs/92)、0.23%(盘点 agent#1)、0.25%(QC 复算)、0.577%(365 clip 实测)。
量级差一个数量级, 而 `aim_share` 的阈值是 1% —— 也就是说**这条判据在所有口径下
都失败**, 它区分不出 session 的好坏, 只在重复宣告一个已知的采集侧缺陷。
因此:

- 定性结论可以引用: aim ≪ attack、toggle-zoom 坏标签、**进训练前 mask 该通道**;
- 具体数值一律不引用;
- aim 相关三项(`aim_share` / `toggle_zoom` / 通道表里的 `combat.aim`)可以用
  `include_aim_checks=False` **单独整组关掉**, 与档位选择正交。

⚠ **两档的过闸率本身也随抽样漂移.** doc 50 §2.4 记 floor 45/122 session、8.960 h,
那是 **365 clip 分层抽样**跑出来的; 用同一个 floor 定义在**全量 8,663 clip 普查**上
重跑得到 **57/122、11.48 h**(若 `channel_completeness` 仍按六通道则 43/122、9.13 h),
strict 在全量普查上是 **0/122**(doc 50 记 1/122、0.173 h)。差异全部来自抽样规模,
不是判据变了。**引用过闸小时数时必须同时说清是哪份样本。**

## 阈值(两档共用, 出处 116 §8.3)

    无输入帧 ≤15% · 指针非零帧 ≥30%(目标 ≥40%) · 纯 W 帧 <15% · 键覆盖 ≥8 ·
    duplicate ≤2% · 动作熵 ≥0.75 · combat.aim ≥1% · combat.primary_attack ≥1% ·
    toggle-zoom aim ≥ 0.2×attack · 冻结通道每会话齐全

`pointer_nonzero` 用的是 `pointerDx/Dy`, 它已被转换器 clamp 到 ±200
(`contract.ActionFrame` docstring)。**这对本判据无害** —— clamp 改的是幅度,
不改"是否为零", 而本判据只问后者。需要真实幅度的下游(IDM 的鼠标回归头)必须从
`events.jsonl` 重算, 那不是本模块的事。

## 边界: 本模块不做什么

- **不做 IO**(除 `load_report` / `dump_report` 两个薄 JSON 壳)。下载、解压、
  列对象全部留给调用方 —— 运维脚本走 rclone, 适配器走 boto3, 判据只有一份。
- **不解码视频**。黑屏段 / 菜单段 / Alt-Tab 段在帧元数据上看不出来
  (doc 50 §6 明记这是本轮未覆盖项), 本模块同样看不出来。
- **不查 `view_scale` 标定**。那是 catalog/H-1 层面的闸
  (`clip_trainability` 的 `view_scale_unverified`), 不是分布问题。
- **不按 `game_id` 过滤**。车道里混着 cod/w2/wuchang/depth 孤儿(doc 50 §6 R1–R3),
  过滤必须由调用方按 `meta.target.game_id` 做 —— 本模块只看 frames 行, 看不到 meta。

## 报告 schema(`halo-qc-report/1`, 稳定契约)

`dump_report` 写出 / `load_report` 读入的 JSON 顶层形状。**这是 `halo_adapter.py`
`--qc-report` 消费的契约, 加字段可以, 改/删已有字段要同时升 `schema` 版本号。**

```jsonc
{
  "schema": "halo-qc-report/1",       // 固定字符串, 读时校验
  "tier": "floor",                    // "floor" | "strict"
  "fps": 60.0,                        // 折算 hours 用的帧率
  "policy": {
    "tier": "floor",
    "enabled_checks": ["empty_frames", "pointer_nonzero", ...],  // 参与总判定的 id
    "channels": ["move.forward", ...],          // 本档要求齐全的冻结通道
    "thresholds": {"max_empty_frac": 0.15, ...} // QCThresholds 的全部字段
  },
  "totals": {
    "sessions": 122, "passed_sessions": 57,
    "clips": 8663, "frames": 5155139,
    "hours": 23.8664, "passed_hours": 11.4831
  },
  "sessions": [                        // 列表, 顺序即写入顺序; 允许同 id 重复(批级报告)
    {
      "session_id": "021930b8-...",
      "scope": "session",              // "session" | "batch"
      "tier": "floor",
      "passed": false,
      "failed_checks": ["empty_frames"],   // 仅 enabled 且未过的 id
      "clips": 98, "frames": 57206, "hours": 0.2648,
      "stats": {
        "frames": 57206, "empty_frames": 9299, "pointer_nonzero_frames": 29662,
        "pure_w_frames": 5423, "duplicate_frames": 41,
        "key_coverage": 17, "keys_seen": ["16", "17", ...], "buttons_seen": [0, 1],
        "action_frames": {"move.forward": 21234, ...},
        "channels_seen": ["move.forward", ...],
        "missing_channels": ["combat.aim"],  // 相对本档 policy.channels 的并集(人读)
        "missing_channels_by_session": {"021930b8-...": ["combat.aim"]}  // 精确映射
      },
      "checks": [                      // 恒 10 条, 顺序恒为 CHECK_ORDER
        {"id": "empty_frames", "label": "无输入帧 ≤15%",
         "value": 0.1626, "threshold": 0.15, "comparison": "<=",
         "reading": "16.3%", "passed": false, "enabled": true, "source": "116 §8.3"}
      ]
    }
  ]
}
```

`checks` 里**恒含全部 10 条**(含本档关掉的), 关掉的 `enabled=false` 且不进
`failed_checks`。`value` 为 `null` 表示该读数无定义(例: attack 帧数为 0 时的
toggle-zoom 比值)。

## 用法

    from diogenes_wm.halo_qc import evaluate_session, dump_report, load_report

    qcs = [evaluate_session(clips_of(sid), session_id=sid, tier="floor")
           for sid in sessions]
    dump_report("qc.json", qcs, tier="floor")

    # 适配器侧
    rep = load_report("qc.json")
    if rep.is_passed(session_id):
        assemble(session_id)
"""

from __future__ import annotations

import json
import math
from collections import Counter
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

__all__ = [
    "FROZEN_CHANNELS",
    "AIM_CHANNEL",
    "CHECK_ORDER",
    "AIM_CHECKS",
    "FLOOR_ONLY_DISABLED",
    "DEFAULT_FPS",
    "REPORT_SCHEMA",
    "QCThresholds",
    "QCPolicy",
    "policy_for",
    "Check",
    "ClipStats",
    "SessionQC",
    "QCReport",
    "evaluate_clip",
    "merge_stats",
    "aggregate",
    "evaluate_session",
    "evaluate_batch",
    "report_to_dict",
    "report_from_dict",
    "dump_report",
    "load_report",
]

# wm01 冻结六通道 (ml/diogenes_wm/wm01/action_space.py, 指纹 c069167cfbc965f2).
# 顺序是承重的: 换顺序会让已训的 LoRA / checkpoint 静默错位。
FROZEN_CHANNELS: tuple[str, ...] = (
    "move.forward",
    "move.backward",
    "move.left",
    "move.right",
    "combat.aim",
    "combat.primary_attack",
)
AIM_CHANNEL = "combat.aim"
ATTACK_CHANNEL = "combat.primary_attack"

W_VK = 87  # 'W' 的 virtual-key code; 纯 W 帧判据用

DEFAULT_FPS = 60.0  # recording profile v3 的 Halo 产线恒 60fps
REPORT_SCHEMA = "halo-qc-report/1"

# 报告里 checks 的固定顺序 —— 与 116 §8.3 的行文顺序一致, 也与运维脚本的打印顺序一致
CHECK_ORDER: tuple[str, ...] = (
    "empty_frames",
    "pointer_nonzero",
    "pure_w",
    "key_coverage",
    "duplicate",
    "action_entropy",
    "aim_share",
    "attack_share",
    "toggle_zoom",
    "channel_completeness",
)

# aim 相关判据, 可用 include_aim_checks=False 整组关掉(见模块 docstring 的 D5 说明).
# 注意 channel_completeness 不在这组里 —— 关掉 aim 时它不是被禁用, 而是必备通道降为五个。
AIM_CHECKS: frozenset[str] = frozenset({"aim_share", "toggle_zoom"})

# floor 相对 strict 额外关掉的(除 AIM_CHECKS 之外)
FLOOR_ONLY_DISABLED: frozenset[str] = frozenset({"action_entropy"})


@dataclass(frozen=True)
class QCThresholds:
    """116 §8.3 的阈值。**两档共用同一组值**, 档位只差哪些判据参与判定。"""

    max_empty_frac: float = 0.15
    min_pointer_nonzero_frac: float = 0.30
    target_pointer_nonzero_frac: float = 0.40  # 116 §8.3 的"目标", 不是硬线
    max_pure_w_frac: float = 0.15  # 严格小于
    min_key_coverage: int = 8
    max_duplicate_frac: float = 0.02
    min_action_entropy: float = 0.75
    min_aim_frac: float = 0.01
    min_attack_frac: float = 0.01
    min_toggle_zoom_ratio: float = 0.20  # docs/111 §5

    def to_dict(self) -> dict[str, float]:
        return {
            "max_empty_frac": self.max_empty_frac,
            "min_pointer_nonzero_frac": self.min_pointer_nonzero_frac,
            "target_pointer_nonzero_frac": self.target_pointer_nonzero_frac,
            "max_pure_w_frac": self.max_pure_w_frac,
            "min_key_coverage": self.min_key_coverage,
            "max_duplicate_frac": self.max_duplicate_frac,
            "min_action_entropy": self.min_action_entropy,
            "min_aim_frac": self.min_aim_frac,
            "min_attack_frac": self.min_attack_frac,
            "min_toggle_zoom_ratio": self.min_toggle_zoom_ratio,
        }

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "QCThresholds":
        known = {k: d[k] for k in cls().to_dict() if k in d}
        return cls(**known)  # type: ignore[arg-type]


@dataclass(frozen=True)
class QCPolicy:
    """一档判据 = 参与判定的 check id 集合 + 必备通道表 + 阈值。"""

    tier: str
    enabled_checks: frozenset[str]
    channels: tuple[str, ...]
    thresholds: QCThresholds = field(default_factory=QCThresholds)

    def without_aim(self) -> "QCPolicy":
        """关掉 aim 相关判据, 并把 `combat.aim` 从必备通道里摘掉(六通道降五)。"""
        return replace(
            self,
            enabled_checks=frozenset(self.enabled_checks) - AIM_CHECKS,
            channels=tuple(c for c in self.channels if c != AIM_CHANNEL),
        )

    def with_aim(self) -> "QCPolicy":
        """把 aim 相关判据加回来, 必备通道恢复冻结六通道的顺序。"""
        return replace(
            self,
            enabled_checks=frozenset(self.enabled_checks) | AIM_CHECKS,
            channels=FROZEN_CHANNELS,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "tier": self.tier,
            "enabled_checks": [c for c in CHECK_ORDER if c in self.enabled_checks],
            "channels": list(self.channels),
            "thresholds": self.thresholds.to_dict(),
        }

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "QCPolicy":
        return cls(
            tier=str(d["tier"]),
            enabled_checks=frozenset(d["enabled_checks"]),
            channels=tuple(d["channels"]),
            thresholds=QCThresholds.from_dict(d.get("thresholds", {})),
        )


STRICT = QCPolicy(
    tier="strict",
    enabled_checks=frozenset(CHECK_ORDER),
    channels=FROZEN_CHANNELS,
)
FLOOR = QCPolicy(
    tier="floor",
    enabled_checks=frozenset(CHECK_ORDER) - AIM_CHECKS - FLOOR_ONLY_DISABLED,
    channels=tuple(c for c in FROZEN_CHANNELS if c != AIM_CHANNEL),
)
TIERS: dict[str, QCPolicy] = {"floor": FLOOR, "strict": STRICT}


def policy_for(
    tier: str | QCPolicy = "floor",
    *,
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
) -> QCPolicy:
    """解析档位。

    `tier` 可以是 `"floor"` / `"strict"` 或一个现成的 `QCPolicy`。
    `include_aim_checks` 与档位**正交**: `True` 强制打开 aim 三项(含六通道),
    `False` 强制关掉, `None` 用该档的默认(floor 关 / strict 开)。
    """
    if isinstance(tier, QCPolicy):
        policy = tier
    else:
        try:
            policy = TIERS[tier]
        except KeyError:
            raise ValueError(
                f"未知档位 {tier!r}; 可选 {sorted(TIERS)} 或一个 QCPolicy"
            ) from None
    if thresholds is not None:
        policy = replace(policy, thresholds=thresholds)
    if include_aim_checks is True:
        policy = policy.with_aim()
    elif include_aim_checks is False:
        policy = policy.without_aim()
    return policy


def policy_for_game(
    game_id: str,
    tier: str = "floor",
    *,
    thresholds: QCThresholds | None = None,
    catalog_root: str | None = None,
) -> QCPolicy:
    """按 game_registry 的词表给任意已注册游戏生成 QC 档.

    与 Halo 常量的关系: `policy_for_game("halo-infinite-steam-release-test",
    "strict").channels == FROZEN_CHANNELS`(test_game_registry 里有一致性断言)。

    非 Halo 游戏的差异只有两处, 都是**词表事实**而非新阈值:
    - `channels` 换成该游戏 mapping 推导的行为地板通道(game_registry.qc_channels);
    - 词表里没有 `combat.aim` 的游戏, aim 三项自动不适用(等价 include_aim_checks=False)。
    阈值(空帧/纯前进/覆盖度/duplicate)沿用 116 §8.3 —— 它们写的是"人在正常玩游戏"
    的行为地板, 不是 Halo 特有的; 若某游戏(如 RTS/驾驶)实测证明阈值口径不适配,
    在调用点显式传 `thresholds`, 并把依据留在调用点。
    """
    from .game_registry import lane_policy, qc_channels

    entry = lane_policy(game_id, catalog_root)
    base = policy_for(tier, thresholds=thresholds)
    chans = qc_channels(entry)
    if not entry.has_combat_aim:
        base = base.without_aim()
        chans = tuple(c for c in chans if c != AIM_CHANNEL)
    elif tier == "floor":
        chans = tuple(c for c in chans if c != AIM_CHANNEL)
    return replace(base, channels=chans)


@dataclass(frozen=True)
class Check:
    """一条判据的读数与判定。`enabled=False` 的仍然算, 只是不进 `failed_checks`。"""

    id: str
    label: str
    value: float | None
    threshold: float | None
    comparison: str  # "<=" | ">=" | "<" | "==" | "n/a"
    reading: str  # 人读格式, 运维脚本直接打印这一列
    passed: bool
    enabled: bool
    source: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "label": self.label,
            "value": self.value,
            "threshold": self.threshold,
            "comparison": self.comparison,
            "reading": self.reading,
            "passed": self.passed,
            "enabled": self.enabled,
            "source": self.source,
        }

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "Check":
        return cls(
            id=str(d["id"]),
            label=str(d["label"]),
            value=d.get("value"),
            threshold=d.get("threshold"),
            comparison=str(d.get("comparison", "n/a")),
            reading=str(d.get("reading", "")),
            passed=bool(d["passed"]),
            enabled=bool(d.get("enabled", True)),
            source=str(d.get("source", "")),
        )


@dataclass(frozen=True)
class ClipStats:
    """一份 `frames.jsonl` 的帧级统计。可相加, 相加即 session/批级聚合。

    这是本模块唯一需要原始数据的地方 —— 之后所有判定都只看 ClipStats。
    """

    clips: int = 0
    frames: int = 0
    empty_frames: int = 0
    pointer_nonzero_frames: int = 0
    pure_w_frames: int = 0
    duplicate_frames: int = 0
    keys_seen: frozenset[int] = frozenset()
    buttons_seen: frozenset[int] = frozenset()
    action_frames: Mapping[str, int] = field(default_factory=dict)

    @property
    def key_coverage(self) -> int:
        """键覆盖 = 不同虚拟键 ∪ 不同鼠标键的个数(与 116 §8.3 的口径一致)。"""
        return len(self.keys_seen) + len(self.buttons_seen)

    @property
    def channels_seen(self) -> frozenset[str]:
        return frozenset(a for a in self.action_frames if a in FROZEN_CHANNELS)

    def missing_channels(self, channels: Sequence[str] = FROZEN_CHANNELS) -> list[str]:
        seen = self.channels_seen
        return [c for c in channels if c not in seen]

    def action_frac(self, action: str) -> float:
        return _frac(self.action_frames.get(action, 0), self.frames)

    def __add__(self, other: "ClipStats") -> "ClipStats":
        if not isinstance(other, ClipStats):
            return NotImplemented
        merged: Counter[str] = Counter(self.action_frames)
        merged.update(other.action_frames)
        return ClipStats(
            clips=self.clips + other.clips,
            frames=self.frames + other.frames,
            empty_frames=self.empty_frames + other.empty_frames,
            pointer_nonzero_frames=self.pointer_nonzero_frames
            + other.pointer_nonzero_frames,
            pure_w_frames=self.pure_w_frames + other.pure_w_frames,
            duplicate_frames=self.duplicate_frames + other.duplicate_frames,
            keys_seen=self.keys_seen | other.keys_seen,
            buttons_seen=self.buttons_seen | other.buttons_seen,
            action_frames=dict(merged),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "clips": self.clips,
            "frames": self.frames,
            "empty_frames": self.empty_frames,
            "pointer_nonzero_frames": self.pointer_nonzero_frames,
            "pure_w_frames": self.pure_w_frames,
            "duplicate_frames": self.duplicate_frames,
            "key_coverage": self.key_coverage,
            "keys_seen": sorted(self.keys_seen),
            "buttons_seen": sorted(self.buttons_seen),
            "action_frames": dict(sorted(self.action_frames.items())),
            "channels_seen": [c for c in FROZEN_CHANNELS if c in self.channels_seen],
        }

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "ClipStats":
        return cls(
            clips=int(d.get("clips", 0)),
            frames=int(d["frames"]),
            empty_frames=int(d.get("empty_frames", 0)),
            pointer_nonzero_frames=int(d.get("pointer_nonzero_frames", 0)),
            pure_w_frames=int(d.get("pure_w_frames", 0)),
            duplicate_frames=int(d.get("duplicate_frames", 0)),
            keys_seen=frozenset(int(k) for k in d.get("keys_seen", ())),
            buttons_seen=frozenset(int(b) for b in d.get("buttons_seen", ())),
            action_frames=dict(d.get("action_frames", {})),
        )


@dataclass(frozen=True)
class SessionQC:
    """一个 session(或一批 session 汇总)的判定结果。

    `scope="session"` 时 `key_coverage` 判据读的是本 session 的键覆盖;
    `scope="batch"` 时读的是**各 session 键覆盖的中位数**, 且
    `channel_completeness` 读的是"缺通道的 session 个数" —— 这两条是
    116 §8.3 里唯二的 session 级(而非帧级)判据, 批级汇总必须换口径,
    否则把所有 session 的键并起来会让键覆盖判据永远过。
    """

    session_id: str
    tier: str
    scope: str  # "session" | "batch"
    stats: ClipStats
    checks: tuple[Check, ...]
    fps: float = DEFAULT_FPS
    n_sessions: int = 1
    missing_channels: Mapping[str, list[str]] = field(default_factory=dict)

    @property
    def clips(self) -> int:
        return self.stats.clips

    @property
    def frames(self) -> int:
        return self.stats.frames

    @property
    def hours(self) -> float:
        return self.stats.frames / self.fps / 3600.0 if self.fps else 0.0

    @property
    def failed_checks(self) -> list[str]:
        return [c.id for c in self.checks if c.enabled and not c.passed]

    @property
    def passed(self) -> bool:
        return not self.failed_checks

    def check(self, check_id: str) -> Check:
        for c in self.checks:
            if c.id == check_id:
                return c
        raise KeyError(check_id)

    def to_dict(self) -> dict[str, Any]:
        stats = self.stats.to_dict()
        # 便于人读的并集; 精确的每 session 映射另存, 保证 round-trip 无损
        stats["missing_channels"] = sorted(
            {c for miss in self.missing_channels.values() for c in miss}
        )
        stats["missing_channels_by_session"] = {
            k: list(v) for k, v in sorted(self.missing_channels.items())
        }
        return {
            "session_id": self.session_id,
            "scope": self.scope,
            "tier": self.tier,
            "passed": self.passed,
            "failed_checks": self.failed_checks,
            "clips": self.clips,
            "frames": self.frames,
            "hours": self.hours,
            "n_sessions": self.n_sessions,
            "stats": stats,
            "checks": [c.to_dict() for c in self.checks],
        }

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "SessionQC":
        stats = ClipStats.from_dict({**d["stats"], "clips": d.get("clips", 0)})
        return cls(
            session_id=str(d.get("session_id", "")),
            tier=str(d.get("tier", "")),
            scope=str(d.get("scope", "session")),
            stats=stats,
            checks=tuple(Check.from_dict(c) for c in d.get("checks", ())),
            fps=_fps_from(d),
            n_sessions=int(d.get("n_sessions", 1)),
            missing_channels={
                k: list(v)
                for k, v in (d["stats"].get("missing_channels_by_session") or {}).items()
            },
        )


def _fps_from(d: Mapping[str, Any]) -> float:
    """报告里 hours 与 frames 都在, 反推 fps 比默认值更忠实。"""
    if d.get("fps"):
        return float(d["fps"])
    hours, frames = d.get("hours"), d.get("frames")
    if hours and frames:
        return frames / (float(hours) * 3600.0)
    return DEFAULT_FPS


@dataclass(frozen=True)
class QCReport:
    """`load_report` 的返回值 —— 适配器的选片依据。"""

    tier: str
    policy: QCPolicy
    fps: float
    sessions: tuple[SessionQC, ...]
    schema: str = REPORT_SCHEMA

    @property
    def by_session(self) -> dict[str, SessionQC]:
        """按 session_id 索引。同 id 多条时保留最后一条。"""
        return {s.session_id: s for s in self.sessions if s.session_id}

    @property
    def passed_session_ids(self) -> frozenset[str]:
        return frozenset(s.session_id for s in self.sessions if s.passed and s.session_id)

    def is_passed(self, session_id: str, *, default: bool = False) -> bool:
        """报告里没有该 session 时返回 `default`(默认 False = 不装配未体检的数据)。"""
        s = self.by_session.get(session_id)
        return default if s is None else s.passed

    @property
    def totals(self) -> dict[str, Any]:
        passed = [s for s in self.sessions if s.passed]
        return {
            "sessions": len(self.sessions),
            "passed_sessions": len(passed),
            "clips": sum(s.clips for s in self.sessions),
            "frames": sum(s.frames for s in self.sessions),
            "hours": sum(s.hours for s in self.sessions),
            "passed_hours": sum(s.hours for s in passed),
        }


# ---------------------------------------------------------------- 帧级统计


def _frac(num: int, den: int) -> float:
    return num / den if den else 0.0


def evaluate_clip(frames_rows: Iterable[Mapping[str, Any]]) -> ClipStats:
    """把一份 `frames.jsonl` 的行(已 json.loads 的 dict)折成 `ClipStats`。

    只读这些字段: `keys` / `buttons` / `pointerDx` / `pointerDy` /
    `duplicate` / `actions`。缺字段按空/零处理 —— 语料里 `actions` 为空、
    `events` 为空帧都是真实存在的形态(doc 50: 128 个 clip 的 events 是空 zstd 帧,
    frames 仍完整 600 行), 不是损坏。

    ⚠ `pointerDx/Dy` 已被 clamp 到 ±200; 本函数只用其"是否非零", 不受 clamp 影响。
    """
    clips = 0 if frames_rows is None else 1
    frames = empty = ptr = pure_w = dup = 0
    keys_seen: set[int] = set()
    buttons_seen: set[int] = set()
    actions: Counter[str] = Counter()

    for row in frames_rows:
        keys = list(row.get("keys") or ())
        buttons = list(row.get("buttons") or ())
        dx = row.get("pointerDx") or 0
        dy = row.get("pointerDy") or 0
        frames += 1
        if row.get("duplicate"):
            dup += 1
        if not (keys or buttons or dx or dy):
            empty += 1
        if dx or dy:
            ptr += 1
        if keys == [W_VK] and not buttons and not dx and not dy:
            pure_w += 1
        for a in set(row.get("actions") or ()):
            actions[a] += 1
        keys_seen.update(int(k) for k in keys)
        buttons_seen.update(int(b) for b in buttons)

    return ClipStats(
        clips=clips,
        frames=frames,
        empty_frames=empty,
        pointer_nonzero_frames=ptr,
        pure_w_frames=pure_w,
        duplicate_frames=dup,
        keys_seen=frozenset(keys_seen),
        buttons_seen=frozenset(buttons_seen),
        action_frames=dict(actions),
    )


def merge_stats(stats_list: Iterable[ClipStats]) -> ClipStats:
    """把多个 `ClipStats` 相加。空输入返回全零 `ClipStats`。"""
    total = ClipStats()
    for s in stats_list:
        total = total + s
    return total


def normalized_action_entropy(action_frames: Mapping[str, int]) -> float:
    """观测到的语义动作帧频分布的归一化熵。

    ⚠ 分母是 `ln(观测到的动作类数)`, 所以**类数本身影响读数** —— 同一段数据,
    多观测到一类罕见动作会同时抬高分子和分母。116 §8.3 的 0.75 线是按 COD 侧
    27 类语义动作的分布定的; Halo 侧词表不同, 这条线的可比性本身存疑
    (这也是 floor 档关掉它的原因之一)。
    """
    total = sum(action_frames.values())
    if total <= 0:
        return 0.0
    probs = [c / total for c in action_frames.values() if c > 0]
    if len(probs) <= 1:
        return 0.0
    return -sum(p * math.log(p) for p in probs) / math.log(len(probs))


# ---------------------------------------------------------------- 判定


def _build_checks(
    stats: ClipStats,
    policy: QCPolicy,
    *,
    key_coverage: int,
    key_coverage_label: str,
    missing_channels: Mapping[str, list[str]],
    channel_reading: str,
) -> tuple[Check, ...]:
    t = policy.thresholds
    n = stats.frames
    empty = _frac(stats.empty_frames, n)
    ptr = _frac(stats.pointer_nonzero_frames, n)
    pure_w = _frac(stats.pure_w_frames, n)
    dup = _frac(stats.duplicate_frames, n)
    entropy = normalized_action_entropy(stats.action_frames)
    aim = stats.action_frac(AIM_CHANNEL)
    atk = stats.action_frac(ATTACK_CHANNEL)
    ratio = aim / atk if atk else None

    def on(cid: str) -> bool:
        return cid in policy.enabled_checks

    raw = [
        Check("empty_frames", "无输入帧 ≤15%", empty, t.max_empty_frac, "<=",
              f"{empty:.1%}", empty <= t.max_empty_frac, on("empty_frames"),
              "116 §8.3"),
        Check("pointer_nonzero", "指针非零帧 ≥30%", ptr, t.min_pointer_nonzero_frac,
              ">=", f"{ptr:.1%}", ptr >= t.min_pointer_nonzero_frac,
              on("pointer_nonzero"), "116 §8.3（目标 ≥40%）"),
        Check("pure_w", "纯 W 帧 <15%", pure_w, t.max_pure_w_frac, "<",
              f"{pure_w:.1%}", pure_w < t.max_pure_w_frac, on("pure_w"), "116 §8.3"),
        Check("key_coverage", key_coverage_label, float(key_coverage),
              float(t.min_key_coverage), ">=", str(key_coverage),
              key_coverage >= t.min_key_coverage, on("key_coverage"), "116 §8.3"),
        Check("duplicate", "duplicate ≤2%", dup, t.max_duplicate_frac, "<=",
              f"{dup:.2%}", dup <= t.max_duplicate_frac, on("duplicate"), "116 §8.3"),
        Check("action_entropy", "动作熵 ≥0.75", entropy, t.min_action_entropy, ">=",
              f"{entropy:.3f}", entropy >= t.min_action_entropy,
              on("action_entropy"), "116 §8.3"),
        Check("aim_share", "combat.aim ≥1%", aim, t.min_aim_frac, ">=",
              f"{aim:.2%}", aim >= t.min_aim_frac, on("aim_share"),
              "116 §8.3 六通道逐条 ≥1%"),
        Check("attack_share", "combat.primary_attack ≥1%", atk, t.min_attack_frac,
              ">=", f"{atk:.2%}", atk >= t.min_attack_frac, on("attack_share"),
              "116 §8.3"),
        Check("toggle_zoom", "toggle-zoom 签名（aim ≥ 0.2×attack）", ratio,
              t.min_toggle_zoom_ratio, ">=",
              f"aim/atk={ratio if ratio is not None else float('nan'):.2f}",
              atk == 0 or aim >= t.min_toggle_zoom_ratio * atk, on("toggle_zoom"),
              "docs/111 §5：0.01 即坏标签签名"),
        Check("channel_completeness",
              f"{'六' if AIM_CHANNEL in policy.channels else '五'}通道每会话齐全",
              float(len(missing_channels)), 0.0, "==", channel_reading,
              not missing_channels, on("channel_completeness"), "116 §8.3"),
    ]
    order = {cid: i for i, cid in enumerate(CHECK_ORDER)}
    return tuple(sorted(raw, key=lambda c: order[c.id]))


def aggregate(
    stats_list: Iterable[ClipStats],
    *,
    session_id: str = "",
    tier: str | QCPolicy = "floor",
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
    fps: float = DEFAULT_FPS,
) -> SessionQC:
    """把一个 session 的各 clip 统计聚成 session 级判定。

    键覆盖取本 session 全部 clip 的**并集**大小; 通道齐全按本档 `policy.channels`
    判。总帧数为 0 时抛 `ValueError` —— 空 session 不该走到判定, 调用方应先跳过。
    """
    policy = policy_for(tier, include_aim_checks=include_aim_checks,
                        thresholds=thresholds)
    stats = merge_stats(stats_list)
    if stats.frames == 0:
        raise ValueError(
            f"session {session_id!r} 没有任何帧; 空 session 不参与 QC 判定"
        )
    missing = stats.missing_channels(policy.channels)
    missing_map = {session_id: missing} if missing else {}
    checks = _build_checks(
        stats,
        policy,
        key_coverage=stats.key_coverage,
        key_coverage_label=f"键覆盖 ≥{policy.thresholds.min_key_coverage}",
        missing_channels=missing_map,
        channel_reading=("齐全" if not missing else "缺 " + ", ".join(missing)),
    )
    return SessionQC(
        session_id=session_id,
        tier=policy.tier,
        scope="session",
        stats=stats,
        checks=checks,
        fps=fps,
        n_sessions=1,
        missing_channels=missing_map,
    )


def evaluate_session(
    clips: Iterable[ClipStats | Iterable[Mapping[str, Any]]],
    *,
    session_id: str = "",
    tier: str | QCPolicy = "floor",
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
    fps: float = DEFAULT_FPS,
) -> SessionQC:
    """一个 session 的完整判定。**这是适配器该 import 的入口。**

    `clips` 的每个元素可以是已算好的 `ClipStats`, 也可以是一份 `frames.jsonl`
    的行序列(dict 的可迭代对象) —— 后者会先过 `evaluate_clip`。混着给也可以。
    """
    stats = [c if isinstance(c, ClipStats) else evaluate_clip(c) for c in clips]
    return aggregate(
        stats,
        session_id=session_id,
        tier=tier,
        include_aim_checks=include_aim_checks,
        thresholds=thresholds,
        fps=fps,
    )


def evaluate_batch(
    sessions: Mapping[str, Iterable[ClipStats | Iterable[Mapping[str, Any]]]],
    *,
    tier: str | QCPolicy = "floor",
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
    fps: float = DEFAULT_FPS,
    label: str = "",
) -> SessionQC:
    """跨 session 的**批级**判定 —— 运维脚本(`tools/halo-intake-qc/qc.py`)用的口径。

    与 `aggregate` 的两处刻意差别(见 `SessionQC` docstring):

    - `key_coverage` 取各 session 键覆盖的**中位数**, 不是并集;
    - `channel_completeness` 读数是"缺通道的 session 个数", 不是缺了哪几个通道。

    帧级的六项在批级就是把所有 clip 的计数加起来 —— 与逐 session 判定不同,
    批级会让好 session 稀释坏 session。**批级过闸 ≠ 每个 session 都能用**,
    选片必须用 `evaluate_session`。
    """
    policy = policy_for(tier, include_aim_checks=include_aim_checks,
                        thresholds=thresholds)
    per_session: dict[str, ClipStats] = {}
    for sid, clips in sessions.items():
        per_session[sid] = merge_stats(
            c if isinstance(c, ClipStats) else evaluate_clip(c) for c in clips
        )
    total = merge_stats(per_session.values())
    if total.frames == 0:
        raise ValueError("批内没有任何帧")

    covs = sorted(s.key_coverage for s in per_session.values())
    key_median = covs[len(covs) // 2] if covs else 0
    missing_map = {
        sid: s.missing_channels(policy.channels)
        for sid, s in per_session.items()
        if s.missing_channels(policy.channels)
    }
    checks = _build_checks(
        total,
        policy,
        key_coverage=key_median,
        key_coverage_label=f"键覆盖中位数 ≥{policy.thresholds.min_key_coverage}",
        missing_channels=missing_map,
        channel_reading=f"{len(missing_map)} 个会话缺通道",
    )
    return SessionQC(
        session_id=label,
        tier=policy.tier,
        scope="batch",
        stats=total,
        checks=checks,
        fps=fps,
        n_sessions=len(per_session),
        missing_channels=missing_map,
    )


# ---------------------------------------------------------------- 报告读写


def report_to_dict(
    results: Iterable[SessionQC],
    *,
    tier: str | QCPolicy = "floor",
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
    fps: float = DEFAULT_FPS,
) -> dict[str, Any]:
    """纯函数版的报告序列化。schema 见模块 docstring。"""
    policy = policy_for(tier, include_aim_checks=include_aim_checks,
                        thresholds=thresholds)
    sessions = list(results)
    report = QCReport(tier=policy.tier, policy=policy, fps=fps,
                      sessions=tuple(sessions))
    return {
        "schema": REPORT_SCHEMA,
        "tier": policy.tier,
        "fps": fps,
        "policy": policy.to_dict(),
        "totals": report.totals,
        "sessions": [s.to_dict() for s in sessions],
    }


def report_from_dict(d: Mapping[str, Any]) -> QCReport:
    """纯函数版的报告反序列化, 校验 `schema` 字段。"""
    schema = d.get("schema")
    if schema != REPORT_SCHEMA:
        raise ValueError(
            f"报告 schema 是 {schema!r}, 本模块只认 {REPORT_SCHEMA!r}"
        )
    policy = QCPolicy.from_dict(d["policy"])
    fps = float(d.get("fps", DEFAULT_FPS))
    sessions = []
    for sd in d.get("sessions", ()):
        s = SessionQC.from_dict(sd)
        sessions.append(replace(s, fps=fps))
    return QCReport(tier=str(d["tier"]), policy=policy, fps=fps,
                    sessions=tuple(sessions), schema=REPORT_SCHEMA)


def dump_report(
    path: str | Path,
    results: Iterable[SessionQC],
    *,
    tier: str | QCPolicy = "floor",
    include_aim_checks: bool | None = None,
    thresholds: QCThresholds | None = None,
    fps: float = DEFAULT_FPS,
    indent: int = 2,
) -> dict[str, Any]:
    """写 JSON 报告, 返回写出的 dict(便于调用方直接复用而不必回读)。"""
    payload = report_to_dict(results, tier=tier,
                             include_aim_checks=include_aim_checks,
                             thresholds=thresholds, fps=fps)
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(payload, ensure_ascii=False, indent=indent) + "\n",
                 encoding="utf-8")
    return payload


def load_report(path: str | Path) -> QCReport:
    """读 JSON 报告。**这是适配器该 import 的另一个入口。**

        rep = load_report("qc.json")
        rep.is_passed(session_id)      # 未体检的 session 默认 False
        rep.passed_session_ids         # frozenset[str]
        rep.totals["passed_hours"]
    """
    return report_from_dict(json.loads(Path(path).read_text(encoding="utf-8")))
