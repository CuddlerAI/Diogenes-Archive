"""Dataset contract: per-frame action labels + chunk metadata + compliance gate.

On-disk layout is the data_demo contract — chunk directories containing:
    p0.npz | p0.mp4   frames (npz key "frames": [T,H,W,3] uint8)
    p0.jsonl          one ActionFrame per video frame
    meta.json         chunk metadata incl. compliance fields

Compliance rule (mirrors track_a_lam/data_contract.py `is_trainable` and the
prod `is_trainable` gate): product-training data must be first-party, consented
and non-cutscene. The attested lane (rights="first_party_attested", DataMiner
docs/141 §3 route B) is the one deliberate variant: there the consent
requirement is replaced by a non-empty `attestation.waiver_ref` — an owner
WAIVER reference carries eligibility instead of a capture-time consent id.
Data that fails the gate (e.g. gaming-500-hours: third-party,
no license) may ONLY be loaded when the caller explicitly passes
`allow_method_validation_only=True` — and must never feed a product model.

That gate reads booleans the adapter itself wrote, so it can only be as honest
as the adapter. `SOURCE_RULES` below is a second, independent lock in the other
direction: a denylist keyed on the R2 bucket/prefix the bytes came from — a fact
fixed at transfer time that no adapter can restamp. Both locks must pass; the
`deny` tier is not bypassable by `allow_method_validation_only`.
"""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from .keys import PHASE0_LEGACY_KEY_SCHEMAS

PHASE0_CONTRACT_LEGACY = "legacy"
PHASE0_CONTRACT_METHOD_VALIDATION = "method-validation"
# AB-P0 lane (research/35 §3.3): dm-1 chunks consumed DELIBERATELY through the
# frozen 12-code vocabulary, for in-domain pseudo-labelling on owner-attested
# first-party data. It is the inverse of the two legacy lanes: every chunk MUST
# carry a dm-1/per-game mapping, and the governance gate runs with no
# method-validation relaxation — only data that passes the full is_trainable
# four-way AND may enter. The vocabulary reduction (per-game physical vocab →
# SCANNED_VKEYS) stays lossy; the entry point is required to measure and report
# the dropped-key share instead of hiding it (research/21 E7 discipline).
PHASE0_CONTRACT_DM1_ATTESTED = "dm1-attested"
PHASE0_CONTRACT_MODES = (
    PHASE0_CONTRACT_LEGACY,
    PHASE0_CONTRACT_METHOD_VALIDATION,
    PHASE0_CONTRACT_DM1_ATTESTED,
)


@dataclass
class ActionFrame:
    """Per-frame ground-truth input (one line of p0.jsonl).

    Three pointer representations, and they are not interchangeable
    (DataMiner docs/45 §3):

    pointer_dx/dy   `pointerDx/Dy` — already clamped to +-200 by the converter
                    and the source of `pointerBins`. Fine as a low-dimensional
                    feature, wrong as a regression target: a fast flick is
                    over a thousand counts in one frame and saturates here, so
                    an R^2 gate on it is scored against a capped label.
    raw_dx/dy       `pointerRawDx/Dy` — untruncated sum of the frame's
                    mouse_move deltas (#137). Absent on data_demo and
                    gaming-500 chunks, where it falls back to pointer_dx/dy.
    look_*_delta_rad  view-scale-calibrated angle (docs/45, #284). Device- and
                    sensitivity-independent, so this is the only cross-machine
                    comparable form — but it exists only after calibration.
                    `view_scale_valid` is False when it is absent, and the
                    angles stay None rather than 0.0: a zero would read as
                    "the camera did not move" instead of "we do not know".
    """
    frame: int
    keys: list[int]                 # VK codes held at this frame
    pointer_dx: float
    pointer_dy: float
    pointer_bins: tuple[int, int]   # signed foveated bins (x, y)
    raw_dx: float = 0.0
    raw_dy: float = 0.0
    look_yaw_delta_rad: Optional[float] = None
    look_pitch_delta_rad: Optional[float] = None
    view_scale_valid: bool = False

    @classmethod
    def from_json(cls, obj: dict) -> "ActionFrame":
        bins = obj.get("pointerBins") or [0, 0]
        dx = float(obj.get("pointerDx", 0))
        dy = float(obj.get("pointerDy", 0))
        yaw = obj.get("look_yaw_delta_rad")
        pitch = obj.get("look_pitch_delta_rad")
        return cls(
            frame=int(obj["frame"]),
            keys=[int(k) for k in obj.get("keys", [])],
            pointer_dx=dx,
            pointer_dy=dy,
            pointer_bins=(int(bins[0]), int(bins[1])),
            raw_dx=float(obj.get("pointerRawDx", dx)),
            raw_dy=float(obj.get("pointerRawDy", dy)),
            look_yaw_delta_rad=None if yaw is None else float(yaw),
            look_pitch_delta_rad=None if pitch is None else float(pitch),
            view_scale_valid=bool(obj.get("view_scale_valid", False)),
        )


@dataclass
class ChunkMeta:
    """Compliance-relevant subset of a chunk's meta.json."""
    sample_key: str
    game: str
    # "first_party" | "first_party_attested" | "third_party" | "synthetic"
    rights: str
    first_party: bool
    consent_id: Optional[str]
    is_trainable: bool
    is_cutscene: bool
    # attested 车道（DataMiner docs/141 §3 路线 B）的治理指针：
    # {"waiver_ref": "r2://.../WAIVER.json@YYYY-MM-DD"}。仅
    # rights == "first_party_attested" 的 chunk 携带；该车道 consent_id 恒为
    # null，取数资格由 WAIVER 引用承载。
    attestation: Optional[dict] = None
    fps: float = 30.0
    tags: list[str] = field(default_factory=list)
    schema_version: str = ""
    annotation_source: str = ""
    key_schema: str = ""
    mapping_id: str | None = None
    mapping_version: str | None = None
    mapping_content_hash: str | None = None
    # 来源坐标(可选, 见 SOURCE_RULES 那节的"未声明来源"权衡)。
    # 适配器写 meta 时应当填, 但既有语料(data_demo / gaming-500 / D2E /
    # cod_attested)都在这两个字段存在之前落的盘, 所以 None 是合法值。
    source_bucket: str | None = None
    source_prefix: str | None = None
    # 转换期被**有意剔除**的语义动作(halo_adapter 的 masked_actions.semantics)。
    # 空列表 = 没剔过。下游必须能分辨"这个通道在语料里是空的"与"这个通道被我们
    # 自己拿掉了" —— 前者是数据缺陷, 后者是裁决(docs/50 §4.2 的 combat.aim)。
    masked_action_semantics: list[str] = field(default_factory=list)

    @classmethod
    def from_json(cls, obj: dict) -> "ChunkMeta":
        schema_version = str(obj.get("schema_version", ""))
        if schema_version and schema_version not in {"dm-1", "dm-2"}:
            raise ValueError(f"unsupported DataModel schema {schema_version!r}")
        if schema_version == "dm-1" and "catalog" in obj:
            raise ValueError("dm-1 chunk metadata must not contain top-level catalog")
        if schema_version == "dm-2" and "catalog" not in obj:
            raise ValueError("dm-2 chunk metadata must contain top-level catalog")
        catalog = obj.get("catalog")
        if schema_version == "dm-2" and catalog is not None:
            if not isinstance(catalog, dict) or set(catalog) != {"provenance"}:
                raise ValueError(
                    "dm-2 catalog must be null or an object containing only provenance"
                )
            if catalog.get("provenance") not in {"compiled", "delivered"}:
                raise ValueError("unsupported dm-2 catalog provenance")
        attestation = obj.get("attestation")
        if attestation is not None and not isinstance(attestation, dict):
            raise ValueError("chunk attestation must be null or an object")
        video = obj.get("video") or {}
        provenance = obj.get("provenance") or {}
        src_bucket, src_prefix = parse_declared_source(obj)
        # fps is load-bearing (window/action semantics scale with it), so a
        # missing declaration is a hard error — never silently assume 30
        # (mixed-fps corpus guard, DataMiner docs/50 P-4). Prefer the rational
        # truth fps_num/fps_den: the integer fps is an approximation, and
        # 59.94 (60000/1001) rounds to the same 60 as a true 60.00 — exactly
        # the silent mix this guard exists to stop (docs/50 P-8).
        num, den = video.get("fps_num"), video.get("fps_den")
        if isinstance(num, (int, float)) and isinstance(den, (int, float)) \
                and num > 0 and den > 0:
            fps_raw = num / den
        else:
            fps_raw = video.get("fps", obj.get("fps"))
        if fps_raw is None:
            raise ValueError(
                f"chunk meta {obj.get('sample_key', '<unknown>')!r} declares no fps "
                "(neither video.fps_num/fps_den, video.fps nor top-level fps); "
                "refusing to assume 30"
            )
        return cls(
            sample_key=str(obj.get("sample_key", "")),
            game=str(obj.get("game", "unknown")),
            rights=str(obj.get("rights", "third_party")),
            first_party=bool(obj.get("first_party", False)),
            consent_id=obj.get("consent_id"),
            is_trainable=bool(obj.get("is_trainable", False)),
            is_cutscene=bool(obj.get("is_cutscene", False)),
            attestation=attestation,
            fps=float(fps_raw),
            tags=list(obj.get("tags", [])),
            schema_version=schema_version,
            annotation_source=str(obj.get("annotation_source", "")),
            key_schema=str(obj.get("key_schema", "")),
            mapping_id=provenance.get("mapping_id") or obj.get("mapping_id"),
            mapping_version=(provenance.get("mapping_version")
                             or obj.get("mapping_version")),
            mapping_content_hash=(provenance.get("mapping_content_hash")
                                  or obj.get("mapping_content_hash")),
            source_bucket=src_bucket,
            source_prefix=src_prefix,
            masked_action_semantics=[
                str(x) for x in
                ((obj.get("masked_actions") or {}).get("semantics") or [])],
        )

    @property
    def source_declared(self) -> bool:
        return bool(self.source_bucket)

    @property
    def uses_per_game_mapping(self) -> bool:
        """Whether this chunk carries the new dm-1/per-game input contract.

        Phase 0 still projects through its fixed legacy key list.  A mapping
        identity or the converter's explicit key-schema marker is therefore a
        hard generation boundary, not a hint that may be ignored.
        """
        return bool(
            self.mapping_id
            or self.mapping_version
            or self.mapping_content_hash
            or "dm-1 mapping" in self.key_schema.lower()
            or self.schema_version in {"dm-1", "dm-2"}
        )

    @property
    def uses_supported_legacy_key_schema(self) -> bool:
        return self.key_schema in PHASE0_LEGACY_KEY_SCHEMAS


def is_trainable_meta(meta: ChunkMeta) -> bool:
    """The product-training gate. Synthetic data counts as first-party consented."""
    if meta.rights == "synthetic":
        return not meta.is_cutscene
    if meta.rights == "first_party_attested":
        # 具结车道（DataMiner docs/141 §3 路线 B）：取数资格由 owner 具结 WAIVER
        # 承载，不要求采集时点 consent_id（该车道 consent_id 恒为 null）。
        # attestation.waiver_ref 缺失或为空 = 资格链断裂，拦下。
        waiver_ref = (meta.attestation or {}).get("waiver_ref")
        return (
            meta.first_party
            and isinstance(waiver_ref, str)
            and bool(waiver_ref.strip())
            and meta.is_trainable
            and not meta.is_cutscene
        )
    return (
        meta.first_party
        and meta.consent_id is not None
        and not meta.is_cutscene
        and meta.is_trainable
    )


class ComplianceError(RuntimeError):
    pass


# ---------------------------------------------------------------------------
# 来源级黑名单 (docs/50 §6.1 R5 / P2)
#
# `is_trainable_meta()` 那四项与是**自陈**的: 它读的是适配器自己写进 meta.json 的
# 布尔。R5 的实测是这条自陈链今天只有一处代码级支撑 ——
# `ml/diogenes_phase0/data/d2e.py:417/445` 硬写 `is_trainable=False`; 而
# csgo-deathmatch / gf-minecraft / vpt-contractor(占 `diogenes-external-datasets`
# 桶 86%)三份**没有任何 adapter**, 其 MANIFEST 里的 `is_trainable:false` 是文档
# 字段, 全仓没有任何代码读它。也就是说今天的保护来自"没人写取数通路", 不是闸:
# 明天谁新写一个适配器, 得靠他自己记得盖戳, 盖错了不会有任何东西报错。
#
# 这张表是**独立于四项与的第二把锁**, 不是它的替代:
#   - 四项与是**正向**判据(什么可以训), 由数据自陈, 因此可以被写错;
#   - 这张表是**反向**判据(哪批字节永远不可训), 由代码钉死在**来源坐标**
#     (R2 bucket + key 前缀)上 —— 而来源坐标是搬运时就定下的、适配器改不了的事实。
# 两把锁都要过。`deny` 档**不可**被 `allow_method_validation_only=True` 绕过 ——
# 那个开关放宽的是"我们自己的、治理链不全的语料", 不是别人的版权。
#
# ## "未知来源"的默认行为: 放行, 且这是有意的
#
# 权衡的两端:
#   - **未知即拒(fail-closed)** 在合规闸上是教科书答案, 但今天落地它等于让
#     **每一份在盘语料**(data_demo / gaming-500 / D2E / synthetic / cod_attested)
#     一次性全部失效 —— 这些字段是今天才加的, 没有任何既有 chunk 带它。一道在
#     100% 既有语料上报错的闸, 会在一天之内被人加 `--no-source-gate` 关掉或者
#     被随手填一个假 bucket 糊过去, 那时它保护的是 0。
#   - **未知即放行(fail-open)** 的代价是: 这张表挡不住"来源没申报"的新适配器。
#     但那正是**四项与**的辖区 —— 不申报来源的新适配器仍然必须自己写出
#     first_party ∧ consent_id ∧ ¬cutscene ∧ is_trainable 才过得去第一把锁,
#     而 R5 要防的具体事故(有人给 VPT/CS:GO 写适配器并盖上 trainable 戳)恰恰
#     会带着来源坐标 —— 你不可能不知道自己在从哪个桶拉字节。
# 所以默认 fail-open, 但把 fail-closed 留成一个**可显式打开**的参数
# (`require_declared_source=True`), 供新车道从第一天起就要求申报; 迁移方向是
# 各车道逐条打开, 而不是全局一次性翻转。
#
# 另外, 最强的那道保护不在 chunk 落盘之后: 新适配器应当在**下载字节之前**先调
# `assert_source_allowed(bucket, key_prefix)` —— 拒绝发生在花钱的出口流量之前,
# 而且此时来源坐标必然在手。
# ---------------------------------------------------------------------------

SOURCE_DENY = "deny"
SOURCE_METHOD_VALIDATION_ONLY = "method-validation-only"
SOURCE_VERDICTS = (SOURCE_DENY, SOURCE_METHOD_VALIDATION_ONLY)


@dataclass(frozen=True)
class SourceRule:
    """一条来源判定。`prefix=""` 表示整桶。

    `reason` 是给人读的判定理由, `evidence` 是出处 —— 两者都必须填: 一条没有
    出处的 DENY 在三个月后无法被复核, 于是要么被当成迷信绕过, 要么拦住本该放行
    的数据而没人敢动它。
    """
    bucket: str
    prefix: str
    verdict: str
    reason: str
    evidence: str

    def __post_init__(self) -> None:
        if self.verdict not in SOURCE_VERDICTS:
            raise ValueError(
                f"unknown source verdict {self.verdict!r}; expected "
                f"{SOURCE_VERDICTS}")

    @property
    def where(self) -> str:
        return f"{self.bucket}/{self.prefix}" if self.prefix else self.bucket

    def matches(self, bucket: str, prefix: str) -> bool:
        """按**路径段**匹配, 不按字符串前缀。

        字符串前缀会让 `raw/v1/attested` 误命中 `raw/v1/attested-evil`, 反过来
        也会让人以为加了一条规则就盖住了一整棵子树。
        """
        if bucket != self.bucket:
            return False
        if not self.prefix:
            return True
        key = prefix.strip("/")
        rule = self.prefix.strip("/")
        return key == rule or key.startswith(rule + "/")


SOURCE_RULES: tuple[SourceRule, ...] = (
    # --- 外部开源镜像: 整桶拒绝 ------------------------------------------------
    # 桶级兜底放在最前: 明天有人往这个桶里放第六份镜像时, 不需要有人记得来这里
    # 加一行 —— 桶本身就是"外部第三方内容"的语义边界。
    SourceRule(
        bucket="diogenes-external-datasets", prefix="",
        verdict=SOURCE_DENY,
        reason="整桶是外部第三方内容镜像, 四份已知镜像的可训练小时数恒为 0; "
               "桶级拒绝是为了让新落桶的镜像默认被拦, 而不是默认被放行。",
        evidence="docs/50 §2.2 + §6.1 R5/R7; WAIVER.json 的 EXCLUDED 列表",
    ),
    SourceRule(
        bucket="diogenes-external-datasets", prefix="vpt-contractor",
        verdict=SOURCE_DENY,
        reason="OpenAI VPT (contractor 录像)。不是许可真空, 是**主动免责**: "
               "LICENSE 是 1,063 B 标准 MIT 措辞、只覆盖 the Software(代码), "
               "README 另行声明 Minecraft 的知识产权属 Microsoft 而非 OpenAI、"
               "且不就 Minecraft 授予任何知识产权。画面因此从未被授权。",
        evidence="docs/50 §2.2 + 分歧记录 D12(实读上游 LICENSE/README)",
    ),
    SourceRule(
        bucket="diogenes-external-datasets", prefix="vpt-schema-sample",
        verdict=SOURCE_DENY,
        reason="与 vpt-contractor 同一批字节的样本(96/96 媒体对象 basename+size "
               "完全重复), 判定随之。注意该前缀下的 README 自称 2026-08-13 已清桶, "
               "而实测删除量精确为 0 —— 桶里的说明文字不能当判据。",
        evidence="docs/50 §2.2 + §6.1 R6(实时 LIST/HEAD 复核)",
    ),
    SourceRule(
        bucket="diogenes-external-datasets", prefix="csgo-deathmatch",
        verdict=SOURCE_DENY,
        reason="CS:GO deathmatch 镜像。上游 MIT 只覆盖编排代码, 画面是 Valve 资产; "
               "另: 主集动作是 IDM 推断值(infer_a)而非人工真值, 即便许可可谈也不该"
               "当动作真值用。",
        evidence="docs/50 §2.2",
    ),
    SourceRule(
        bucket="diogenes-external-datasets", prefix="d2e-480p",
        verdict=SOURCE_DENY,
        reason="D2E。CC-BY-NC-4.0 的 NC 条款本身禁商用, 是四份里最不可谈判的一条。"
               "这条与 data/d2e.py 硬写的 is_trainable=False 并列存在: 那是适配器"
               "**自陈**, 这是来源坐标上的**他证**, 一处被改掉另一处仍在。",
        evidence="docs/50 §2.2 + §6.1 R5; ml/diogenes_phase0/data/d2e.py:417,445",
    ),
    SourceRule(
        bucket="diogenes-external-datasets", prefix="gf-minecraft",
        verdict=SOURCE_DENY,
        reason="GameFactory Minecraft。上游未声明许可 + Mojang/Microsoft IP; "
               "另: 内容是脚本化随机动作序列, 不含人的策略。",
        evidence="docs/50 §2.2",
    ),
    # --- 我们自己的、治理链不全的语料: 只许方法验证 ------------------------------
    SourceRule(
        bucket="diogenes-training-dev", prefix="method-validation",
        verdict=SOURCE_METHOD_VALIDATION_ONLY,
        reason="永久 method-validation-only: 治理链三字段 20/20 全空, 且**设计上"
               "不可回填**(这批 chunk 落盘时就没有 consent 记录可追)。可以用来验"
               "方法, 产物不得进产品、不得发布权重。",
        evidence="docs/50 §2.3 表 + 分歧记录 D11",
    ),
    SourceRule(
        bucket="diogenes-model-training", prefix="wm01-archive",
        verdict=SOURCE_METHOD_VALIDATION_ONLY,
        reason="与 diogenes-training-dev/method-validation 是**同一批字节**"
               "(19/20 session id 同集、2.356 vs 2.357 h), 却挂着 is_trainable=true "
               "的相反判定; 内容是 Activision IP, 不得发布不得进产品。数据本身分不"
               "出来, 只靠路由到哪条车道区分 —— 所以这条锁挂在来源坐标上。"
               "⚠ 该条目待 owner 裁决(docs/50 R4): 若 owner 确认这批字节可进产品"
               "训练, 删掉本条; 在裁决之前, 保守档位是拦住产品车道。",
        evidence="docs/50 §2.3 + §6.1 R4",
    ),
)


def parse_source_uri(uri: str) -> tuple[str | None, str]:
    """`r2://bucket/key…` / `s3://bucket/key…` / `bucket/key…` → (bucket, prefix)。

    容忍 https 形式的 R2 端点 (`https://<account>.r2.cloudflarestorage.com/
    <bucket>/<key>`), 因为搬运脚本里两种写法都出现过。
    """
    s = (uri or "").strip()
    if not s:
        return None, ""
    for scheme in ("r2://", "s3://", "gs://"):
        if s.startswith(scheme):
            s = s[len(scheme):]
            break
    else:
        if s.startswith("http://") or s.startswith("https://"):
            s = s.split("://", 1)[1]
            # 去掉 host 段, 只留 <bucket>/<key>
            s = s.split("/", 1)[1] if "/" in s else ""
    s = s.strip("/")
    if not s:
        return None, ""
    bucket, _, prefix = s.partition("/")
    return bucket, prefix


def parse_declared_source(obj: dict) -> tuple[str | None, str]:
    """从一份 meta.json 里读来源坐标。三种写法都认, 优先级从高到低:

        {"source": {"bucket": …, "prefix": …}}     # 首选
        {"source_bucket": …, "source_prefix": …}   # 扁平写法
        {"source_uri": "r2://bucket/prefix"}       # 搬运脚本常见
        {"provenance": {"source_bucket": …, …}}    # provenance 块内

    读不到就返回 (None, "") —— "未声明", 不是"声明为空"。
    """
    src = obj.get("source")
    if isinstance(src, dict):
        bucket = src.get("bucket")
        if bucket:
            return str(bucket), str(src.get("prefix") or src.get("key") or "")
        if src.get("uri"):
            return parse_source_uri(str(src["uri"]))
    if isinstance(src, str) and ("://" in src or src.count("/") >= 1):
        # 纯字符串的 `source` 在既有 meta 里是自由文本(cod_adapter 写的是
        # "DiogenesLab/… (local build)"), 所以只在它长得像 URI 时才解析。
        if "://" in src:
            return parse_source_uri(src)
    prov = obj.get("provenance") or {}
    for holder in (obj, prov):
        if holder.get("source_bucket"):
            return (str(holder["source_bucket"]),
                    str(holder.get("source_prefix") or ""))
        if holder.get("source_uri"):
            return parse_source_uri(str(holder["source_uri"]))
    return None, ""


def source_stamp(bucket: str, prefix: str = "") -> dict:
    """适配器写 meta.json 时展开进去的来源戳。

    存在的意义是让"申报来源"有一个唯一写法 —— 三种解析写法是为了兼容既有数据,
    新代码只应该用这一个。
    """
    if not bucket:
        raise ValueError("来源戳必须有 bucket; 不知道从哪来就不要盖戳")
    return {"source": {"bucket": str(bucket), "prefix": str(prefix or "")}}


def match_source_rule(bucket: str | None,
                      prefix: str = "") -> Optional[SourceRule]:
    """命中的规则, 最长前缀优先(整桶兜底因此不会盖住更具体的理由)。"""
    if not bucket:
        return None
    hits = [r for r in SOURCE_RULES if r.matches(bucket, prefix or "")]
    if not hits:
        return None
    return max(hits, key=lambda r: len(r.prefix))


def assert_source_allowed(bucket: str | None, prefix: str = "", *,
                          allow_method_validation_only: bool = False,
                          what: str = "") -> None:
    """来源级闸。新适配器应当在**下载字节之前**先调它。

    `deny` 档不看 `allow_method_validation_only` —— 那个开关放宽的是我们自己
    治理链不全的语料, 不是别人的版权。
    """
    rule = match_source_rule(bucket, prefix)
    if rule is None:
        return
    subject = what or f"{bucket}/{prefix}".rstrip("/")
    if rule.verdict == SOURCE_DENY:
        raise ComplianceError(
            f"来源 `{subject}` 命中永不可训练名单 `{rule.where}`。\n"
            f"理由: {rule.reason}\n出处: {rule.evidence}\n"
            "这一档不可用 allow_method_validation_only 绕过 —— 要改判定, "
            "改的是 contract.SOURCE_RULES 并带上新的出处。"
        )
    if not allow_method_validation_only:
        raise ComplianceError(
            f"来源 `{subject}` 命中 method-validation-only 名单 `{rule.where}`。\n"
            f"理由: {rule.reason}\n出处: {rule.evidence}\n"
            "要在方法验证车道消费它, 显式设 data.contract_mode=method-validation; "
            "产物不得进产品、不得发布权重。"
        )


def gate_sources(metas: list[ChunkMeta], *, allow_method_validation_only: bool,
                 require_declared_source: bool = False) -> None:
    """对一批 chunk 逐条跑来源闸。

    `require_declared_source=True` 把"未声明来源"也变成硬错 —— 默认关, 理由见
    本节开头的权衡。新车道可以从第一天起打开它。
    """
    undeclared = [m.sample_key for m in metas if not m.source_declared]
    if require_declared_source and undeclared:
        raise ComplianceError(
            f"{len(undeclared)} 个 chunk 未申报来源 (如 {undeclared[:3]})。"
            "本车道要求显式声明 source.bucket/prefix(见 contract.source_stamp): "
            "来源坐标是搬运时就定下的事实, 适配器改不了, 因此它是唯一一个可以"
            "钉住合规判定的字段。"
        )
    for m in metas:
        assert_source_allowed(
            m.source_bucket, m.source_prefix or "",
            allow_method_validation_only=allow_method_validation_only,
            what=m.sample_key or f"{m.source_bucket}/{m.source_prefix or ''}",
        )


def resolve_phase0_contract_mode(
    contract_mode: str | None,
    *,
    allow_method_validation_only: bool | None = None,
) -> str:
    """Resolve the explicit Phase 0 lane while keeping old configs readable.

    New configs should set ``data.contract_mode``.  The boolean fallback is
    retained for open/research configs that predate this gate, but conflicting
    declarations fail rather than picking whichever one happens to be read
    first.
    """
    if contract_mode is None:
        return (PHASE0_CONTRACT_METHOD_VALIDATION
                if allow_method_validation_only else PHASE0_CONTRACT_LEGACY)
    if contract_mode not in PHASE0_CONTRACT_MODES:
        raise ComplianceError(
            f"unknown Phase 0 contract_mode {contract_mode!r}; expected one of "
            f"{PHASE0_CONTRACT_MODES}"
        )
    if allow_method_validation_only is not None:
        expects_method = contract_mode == PHASE0_CONTRACT_METHOD_VALIDATION
        if bool(allow_method_validation_only) != expects_method:
            raise ComplianceError(
                "data.contract_mode conflicts with "
                "data.allow_method_validation_only"
            )
    return contract_mode


def gate_phase0_contract(metas: list[ChunkMeta], *, contract_mode: str) -> None:
    """Keep the fixed Phase 0 label space on its two supported legacy lanes.

    Neither legacy lane can consume dm-1/per-game mappings: doing so would
    silently project a larger game-specific physical vocabulary through
    ``keys.py``'s fixed list.  ``method-validation`` only relaxes governance
    eligibility; it never relaxes the input-contract generation boundary.

    ``dm1-attested`` is the one deliberate exception (AB-P0, research/35 §3.3):
    it accepts ONLY dm-1 chunks (mixing legacy chunks in would blend two key
    schemas inside one run) and ONLY data passing the full governance gate.
    The projection through the 12-code list is still lossy — that lane's entry
    point owns reporting the out-of-vocabulary share.
    """
    mode = resolve_phase0_contract_mode(contract_mode)
    if mode == PHASE0_CONTRACT_DM1_ATTESTED:
        unmapped = [m.sample_key for m in metas if not m.uses_per_game_mapping]
        if unmapped:
            raise ComplianceError(
                f"{len(unmapped)} chunk(s) do not carry a dm-1/per-game mapping "
                f"(e.g. {unmapped[:3]}). dm1-attested consumes dm-1 chunks only; "
                "mixing legacy chunks would blend two key schemas in one run."
            )
        # No method-validation relaxation on this lane, ever: it exists for the
        # owner-attested first-party corpus, and pseudo-labels inherit their
        # source's governance.
        gate_chunks(metas, allow_method_validation_only=False)
        return
    mapped = [m.sample_key for m in metas if m.uses_per_game_mapping]
    if mapped:
        raise ComplianceError(
            f"{len(mapped)} chunk(s) use dm-1/per-game input mappings "
            f"(e.g. {mapped[:3]}). Phase 0 is frozen to the legacy physical "
            "vocabulary; use ml/player with a pinned PhysicalInputVocab."
        )
    unknown_schemas = [
        (m.sample_key, m.key_schema or "<missing>")
        for m in metas
        if not m.uses_supported_legacy_key_schema
    ]
    if unknown_schemas:
        raise ComplianceError(
            f"{len(unknown_schemas)} chunk(s) do not declare a supported "
            f"Phase 0 legacy key schema (e.g. {unknown_schemas[:3]}). "
            "Refusing to project an unknown physical vocabulary through "
            "keys.py's frozen 12-code list."
        )
    gate_chunks(
        metas,
        allow_method_validation_only=(
            mode == PHASE0_CONTRACT_METHOD_VALIDATION),
    )


def gate_chunks(metas: list[ChunkMeta], *, allow_method_validation_only: bool,
                require_declared_source: bool = False) -> None:
    """Hard-fail unless every chunk passes the gate or the caller explicitly
    opted into method-validation-only mode.

    两把**并列**的锁, 都要过:

    1. 来源闸 (`gate_sources`) —— 钉在 R2 bucket/前缀这种适配器改不了的事实上,
       `deny` 档连 `allow_method_validation_only=True` 也绕不过去;
    2. 自陈闸 (`is_trainable_meta`) —— 四项与, 读的是 meta.json 里的布尔。

    顺序是先来源后自陈: 一批 VPT 字节被人手工盖上 `is_trainable=true` 时, 报错
    应当说"这个桶永不可训练", 而不是通过。
    """
    gate_sources(metas, allow_method_validation_only=allow_method_validation_only,
                 require_declared_source=require_declared_source)
    blocked = [m.sample_key for m in metas if not is_trainable_meta(m)]
    if blocked and not allow_method_validation_only:
        raise ComplianceError(
            f"{len(blocked)} chunk(s) fail the is_trainable gate "
            f"(e.g. {blocked[:3]}). Set data.allow_method_validation_only=true "
            "ONLY for method validation — never for product training."
        )


def gate_fps(metas: list[ChunkMeta], *, expected_fps: Optional[float] = None) -> None:
    """Hard-fail on mixed-fps corpora (DataMiner docs/50 P-4).

    Window length, pointer-delta magnitude and framepair horizons are all
    frame-denominated, so mixing frame rates inside one training run silently
    changes what the model learns. A run must pin exactly one fps: either every
    chunk agrees, or the caller names the rate via `expected_fps` and every
    chunk must match it.
    """
    if not metas:
        return
    values = sorted({m.fps for m in metas})
    if expected_fps is not None:
        bad = [m.sample_key for m in metas if m.fps != float(expected_fps)]
        if bad:
            raise ComplianceError(
                f"{len(bad)} chunk(s) declare fps != expected {expected_fps} "
                f"(e.g. {bad[:3]}; present values: {values})."
            )
    elif len(values) > 1:
        raise ComplianceError(
            f"mixed-fps corpus: {values}. One training run consumes exactly one "
            "fps — filter the records or set data.expected_fps to pin the rate."
        )


@dataclass
class ChunkRecord:
    """A discovered on-disk chunk (frames + actions + meta paths)."""
    chunk_dir: Path
    meta: ChunkMeta
    n_frames: int

    @property
    def frames_path(self) -> Path:
        npz = self.chunk_dir / "p0.npz"
        return npz if npz.exists() else self.chunk_dir / "p0.mp4"

    @property
    def actions_path(self) -> Path:
        return self.chunk_dir / "p0.jsonl"


def load_actions(path: Path) -> list[ActionFrame]:
    out = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                out.append(ActionFrame.from_json(json.loads(line)))
    return out
