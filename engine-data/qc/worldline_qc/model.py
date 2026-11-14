"""四世界线 QC 的结果模型：规则结果、状态、组级报告。

状态口径（比 engine_qc 多两档，因为三期需求里有大量「待需方确认」与「须人工看画面」的条款）：

- PASS        自动核验通过。
- FAIL        自动核验不通过，硬性条款。
- WARN        已量化、但需方口径未定（如运动像素判定法）或条款是「尽量/建议」；不阻断，报告里必须看得见。
- MANUAL      QC 只能给证据（缩略图、曲线、统计），最终要人看画面判；不阻断。
- SKIP        本路/本组不适用（如 B 线不做共视）。
- UNVERIFIED  该跑没跑（工具链缺失、文件缺失导致无法核验）。与 SKIP 的区别同 engine_qc：SKIP 可放行，UNVERIFIED 不行。

组级判定：FAIL > UNVERIFIED > PASS。WARN / MANUAL 不改变判定，但报告顶部单列。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Dict, List, Optional

PASS = "PASS"
FAIL = "FAIL"
WARN = "WARN"
MANUAL = "MANUAL"
SKIP = "SKIP"
UNVERIFIED = "UNVERIFIED"

STATUSES = (PASS, FAIL, WARN, MANUAL, SKIP, UNVERIFIED)
STATUS_ZH = {PASS: "达标", FAIL: "不达标", WARN: "待确认/提示", MANUAL: "需人工复核",
             SKIP: "不适用", UNVERIFIED: "未能核验"}


@dataclass
class RuleResult:
    rule: str          # 规则 id，如 video.frames / motion.speed_p95
    status: str
    target: str        # 检查对象：lane 标识（如 C/3）、take、group 或 "-"
    evidence: str      # 数值证据 / 原因，中文
    req: str = ""      # 需求条款引用，如 "3.2 文件与时长"
    data: Optional[dict] = None   # 给报告画图用的结构化数据（曲线、直方图等）

    def to_dict(self) -> dict:
        d = {"rule": self.rule, "status": self.status, "target": self.target,
             "evidence": self.evidence, "req": self.req}
        if self.data is not None:
            d["data"] = self.data
        return d


@dataclass
class LaneInfo:
    take: str            # 所在文件夹（AB / C / D，或平铺布局下的 "flat"）
    lane: int            # 文件夹内编号（video_N）
    worldline: str       # A / B / C / D
    role: str            # fixed / orbit / dolly / free_camera / chase / first_person / free（未知）
    focal_mm: Optional[float] = None
    global_index: Optional[int] = None   # 需方 1～12 编号（若可推出）

    @property
    def key(self) -> str:
        return f"{self.take}/{self.lane}"

    @property
    def label(self) -> str:
        g = f"#{self.global_index} " if self.global_index else ""
        return f"{g}{self.take}/video_{self.lane}（世界线{self.worldline}·{ROLE_ZH.get(self.role, self.role)}）"

    def to_dict(self) -> dict:
        return {"take": self.take, "lane": self.lane, "worldline": self.worldline, "role": self.role,
                "focal_mm": self.focal_mm, "global_index": self.global_index}


ROLE_ZH = {"fixed": "固定机位", "orbit": "环绕运镜", "dolly": "横移运镜", "free": "自由运镜",
           "free_camera": "真实输入自由相机", "chase": "第三人称追背", "first_person": "第一人称"}

# 哪些角色是「自由运镜相机」（适用运镜类目 / 双路独立 / 六向平移）
PROGRAM_FREE_ROLES = ("orbit", "dolly", "free")
# 哪些角色带人物字段（与 stage_delivery 的合同一致）
PERSON_ROLES = ("chase", "first_person")
# 哪些角色的键鼠是真实输入（录制时真按的键、真动的鼠标）。键鼠字段每一路都带（需方 2026-09-29）：
# 自由运镜的由运镜路线逐帧换算（非实时操作，键只表示方向），固定机位全程无按键、鼠标为 0。
INPUT_ROLES = ("free_camera", "chase", "first_person")
STILL_ROLES = ("fixed",)
MOVING_ROLES = ("orbit", "dolly", "free", "free_camera", "chase", "first_person")


@dataclass
class GroupReport:
    group: str
    lanes: List[LaneInfo] = field(default_factory=list)
    results: List[RuleResult] = field(default_factory=list)
    meta: Dict[str, object] = field(default_factory=dict)

    def add(self, rule: str, status: str, target: str, evidence: str, req: str = "",
            data: Optional[dict] = None) -> RuleResult:
        r = RuleResult(rule, status, target, evidence, req, data)
        self.results.append(r)
        return r

    def extend(self, results: List[RuleResult]) -> None:
        self.results.extend(results)

    @property
    def counts(self) -> Dict[str, int]:
        c = {s: 0 for s in STATUSES}
        for r in self.results:
            c[r.status] = c.get(r.status, 0) + 1
        return c

    @property
    def verdict(self) -> str:
        if any(r.status == FAIL for r in self.results):
            return FAIL
        if any(r.status == UNVERIFIED for r in self.results):
            return UNVERIFIED
        return PASS

    def by_rule(self, prefix: str) -> List[RuleResult]:
        return [r for r in self.results if r.rule.startswith(prefix)]

    def to_dict(self) -> dict:
        return {
            "group": self.group,
            "verdict": self.verdict,
            "counts": self.counts,
            "lanes": [l.to_dict() for l in self.lanes],
            "meta": self.meta,
            "results": [r.to_dict() for r in self.results],
        }
