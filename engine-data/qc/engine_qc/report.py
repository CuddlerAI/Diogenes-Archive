"""规则结果与报告渲染：人可读文本 + --json 机器格式。"""

from dataclasses import dataclass, field
from typing import Dict, List

PASS = "PASS"
FAIL = "FAIL"
SKIP = "SKIP"
#: 该规则本应跑但**没能跑**（工具链缺失等）。与 SKIP 的区别是：SKIP = 不适用，
#: UNVERIFIED = 适用却没验证过。前者可以放行，后者不行——否则「没检查」会被读成「检查通过」。
UNVERIFIED = "UNVERIFIED"

STATUSES = (PASS, FAIL, SKIP, UNVERIFIED)


@dataclass
class RuleResult:
    rule: str      # 规则 id，如 json.schema / phys.speed_p95
    status: str    # PASS / FAIL / SKIP
    target: str    # 检查对象（文件名 / clip / "-"）
    evidence: str  # 数值证据 / 原因

    def to_dict(self) -> dict:
        return {
            "rule": self.rule,
            "status": self.status,
            "target": self.target,
            "evidence": self.evidence,
        }


@dataclass
class ClipReport:
    clip: str
    mode: str
    results: List[RuleResult] = field(default_factory=list)

    def add(self, rule: str, status: str, target: str, evidence: str) -> None:
        self.results.append(RuleResult(rule, status, target, evidence))

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
        """FAIL > UNVERIFIED > PASS。

        UNVERIFIED 排在 PASS 前面是这个判定的全部意义：有规则该跑没跑，
        就不能对外说「通过」。
        """
        if any(r.status == FAIL for r in self.results):
            return FAIL
        if any(r.status == UNVERIFIED for r in self.results):
            return UNVERIFIED
        return PASS

    def failures(self) -> List[RuleResult]:
        return [r for r in self.results if r.status == FAIL]

    def unverified(self) -> List[RuleResult]:
        return [r for r in self.results if r.status == UNVERIFIED]

    def json_verdict(self) -> str:
        """非 video 规则的判定（抽检协议 json 面使用）。"""
        for r in self.results:
            if r.status == FAIL and not r.rule.startswith("video."):
                return FAIL
        return PASS

    def video_verdict(self) -> str:
        """video 规则判定：FAIL > UNVERIFIED > PASS > SKIP。"""
        seen = unver = False
        for r in self.results:
            if not r.rule.startswith("video."):
                continue
            if r.status == FAIL:
                return FAIL
            if r.status == UNVERIFIED:
                unver = True
            elif r.status == PASS:
                seen = True
        if unver:
            return UNVERIFIED
        return PASS if seen else SKIP

    def to_dict(self) -> dict:
        return {
            "clip": self.clip,
            "mode": self.mode,
            "verdict": self.verdict,
            "counts": self.counts,
            "results": [r.to_dict() for r in self.results],
        }


def render_clip_text(report: ClipReport) -> str:
    lines = [f"== engine-data-qc · clip {report.clip} · mode={report.mode} =="]
    for r in report.results:
        lines.append(f"[{r.status}] {r.rule:<22} {r.target:<28} {r.evidence}")
    c = report.counts
    tail = ""
    if report.verdict == UNVERIFIED:
        tail = ("（有规则该跑未跑："
                + "、".join(sorted({r.rule for r in report.unverified()}))
                + "）")
    lines.append(
        f"-- 共 {len(report.results)} 项：PASS {c[PASS]} / FAIL {c[FAIL]} / "
        f"SKIP {c[SKIP]} / UNVERIFIED {c[UNVERIFIED]} → 判定 {report.verdict}{tail}"
    )
    return "\n".join(lines)
