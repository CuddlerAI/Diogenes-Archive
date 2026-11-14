"""Halo 产品语料(R2 具结车道 · API 摄取布局) → chunk 契约目录, 训练侧的取数入口.

    python -m diogenes_wm.halo_adapter \
        --src r2://diogenes-model-training/raw/v1/attested-trainable/dataminer-api-production/sessions/ \
        --cache /data/halo-cache --out /data/halo-chunks \
        --rights first_party --consent-id <owner 具结 ID> \
        --qc-report qc.json --qc-mode floor

## 这个模块要解决的问题(research/50 §4.1)

具结车道上有 23.87 h Halo 产品语料, 但**训练侧一行代码都取不到**: 仓库的三条装配
通路(`tools/mv-pipeline`、converter 的 `first-party-release` 档、
`ml/diogenes_wm/cod_adapter.py`)全部只吃 manual/parquet 布局, 而这批语料是
**API 摄取的内容寻址布局**。本模块是那条缺失的通路 —— 只做格式转换与选片, 不改
治理结论。

    <src>/<session_uuid>/runs/<run_uuid>/clips/<clip_uuid>/
        meta-<sha>.json        逐 clip 元数据(target/mapping/catalog/probe/health)
        manifest-<sha>.json    {逻辑文件名 → "sha256:<hex>"}
        frames-<sha>.jsonl.zst 逐帧一行, 与视频帧 1:1
        events-<sha>.jsonl.zst 原始输入事件流(qpc/type/dx/dy)
        video-<sha>.mp4
        audio-<sha>.pcm.zst

文件名内嵌 sha256(内容寻址), manifest 与 meta 各存一份哈希 —— **完整性校验是免费
的**, 所以这里默认全验 meta/frames/events, 不做"信任上游"的省事选择。

## 两条车道, 由 `--rights` 选, 默认是不可训练的那条

沿用 `cod_adapter.compliance_stamp()` 的设计(2026-08-01 裁决, research/33 §1.3):

**`--rights third_party`(默认)** 把源数据的合规结论逐字抄下来 —— 具结车道里每个
对象的 meta 至今仍写 `non_trainable=true`(research/50 §3), 所以默认档产出的 chunk
过不了 `contract.is_trainable_meta()` 的四项与, `gate_chunks()` 默认硬拦。

**`--rights first_party --consent-id <具结 ID>`** 才翻车道。三点刻意的设计与 COD
侧完全一致, 理由也一样:

1. **必须同时给 `--consent-id`**, 缺了就报错。门是
   `first_party ∧ consent_id ∧ ¬cutscene ∧ is_trainable` 四项与, 单翻一个只会让人
   以为改了其实没改。
2. **`production_certified` 两条车道恒为 false**。它是产品认证位; 具结说的是
   "验证用、不发布", 那不是产品认证。
3. **约束跟着数据走**, 写进每个 `meta.json` 的 `tags`, 而不是只留在某一次对话里。

### ⚠ Halo 的授权边界与 COD 不同, 标签也不同

COD 是纯第三方内容(版权归 Activision), 具结覆盖的只有"采集与内部验证"。
**Halo 是微软合作授权内容**: C-1 已授权**采集**与**内部训练**。但

* **对外测试/演示/发布需要 C-2**, C-2 尚未取得;
* 因此本模块的具结车道戳 `owner_attested_validation` + `no_publish` +
  `halo_partner_licensed` 三个标签 —— 第三个标签是给三个月后的人看的: 这批数据
  **不是**无主素材, 它带着一份具名的合作授权, 用途边界由那份授权而不是由本仓库
  的默认假设决定;
* `no_publish` 在这条车道上依然成立 —— 它挡的是 C-2 之外的对外使用, 不是内部训练;
* 边界本身**跟着数据走**: 具结车道的每个 `meta.json` 带 `license_boundary` 块
  (授权编号 / 覆盖什么 / 不覆盖什么 / 缺什么)与 `c2_not_obtained`、
  `no_external_use` 标签 —— 只有一个 `halo_partner_licensed` 标签读起来像
  "有授权可以发", 与实际边界方向相反。

产物不得对外分发、不得进任何公开发布的 checkpoint、不得用于对外演示。

**授权按作品成立, 所以 `--game-id` 是承重参数**: 具结车道是混的, 换一个 game_id
去跑 `--rights first_party` 会给一份本模块并不知道其授权状态的内容盖上
"halo 合作授权 + 可训练"。因此 `HALO_PARTNER_GAME_IDS` 之外的作品在具结车道上
**硬报错**, 在默认档上把标签降级为 `license_unknown_non_halo_title`。

## 四件必须在转换期做掉的事

1. **按 `meta.target.game_id` 过滤(强制)**。具结车道是混的: 8,744 clip 里 halo
   8,663, 另有 cod 6 / wolfenstein2 67 / wuchang 7, 外加 **1 个没有 `target` 字段
   的 fallout4 depth 孤儿**(research/50 §6 R1–R3)。孤儿正是"按路径推断游戏"漏进来
   的, 所以这里**只认 meta 里的 `target.game_id`, 缺字段一律拒收**。
2. **fps 同质**。`contract.gate_fps()` 对混合帧率**硬报错**(窗口长度、指针幅度、
   framepair 视野全是按帧计的)。车道里 8,743 clip 是 60/1, 混着 1 个 30/1 —— 默认
   只收 60/1, 其余计数报出。
3. **鼠标必须从 events 重算**。Halo 的 `frames.jsonl` **没有** `pointerRawDx/Dy`
   字段(实测 0/600 行命中), 只有被 converter clamp 到 ±200 的 `pointerDx/Dy`。不
   重算就等于拿一个封顶的标签去打鼠标门(`contract.ActionFrame` docstring /
   DataMiner docs/45 §3)。算法与 COD 侧同构: 按每帧 `[output_qpc_start,
   output_qpc_end)` **半开区间**求和区间内所有 `mouse_move` 的 dx/dy。
   实测该重算与 `frames.jsonl` 自带的逐帧事件摘要在 1,800 帧上 100% 一致, 且恰好
   在 clamp 生效处与 `pointerDx` 分叉(211→200, 243→200) —— 这条一致性被做成了
   `pointer_digest_mismatch_frames` 统计, 每次转换都重跑一遍。
4. **`combat.aim` 必须 mask**(research/50 §4.2 / D5)。它是 toggle-zoom 语义的坏
   标签: 按下切换、再按取消, 于是"按住瞄准"的帧序列在标签里根本不成立。六通道动作
   头因此降五通道。默认 `--mask-actions combat.aim`; 被剔除的语义**逐帧留痕**在
   `actions_masked` 里(可逆), 并在 chunk 的 `meta.json` 写 `masked_actions` 块 ——
   三个月后翻到这批 chunk 的人必须看得见这里动过手。
   注: aim 的**占比**在五个来源上有五个值(0.05%–8%), research/50 D5 的裁决是
   **只保留定性结论、不引用任何一个数值**, 所以这里也不写死阈值, 只做 mask 与计数。

## 来源闸(第二把锁)

`contract.SOURCE_RULES` 是钉在 **R2 bucket + key 前缀**上的反向名单(docs/50 §6.1
R5/P2)。它与四项与并列: 四项与是自陈的(适配器自己写的布尔, 可以写错), 来源坐标是
搬运时就定下的、适配器改不了的事实。本模块两头都接:

* **下载字节之前**先调 `assert_source_allowed(bucket, prefix)` —— 拒绝要发生在花钱
  的出口流量之前, 而且此刻来源坐标必然在手;
* 每个 chunk 的 `meta.json` 用 `contract.source_stamp()` 盖上**该 clip 自己的**
  bucket/前缀, 让 `gate_sources()` 事后还能复核。

本地镜像模式必须用 `--source-uri r2://<bucket>/<prefix>` 申报字节从哪来 —— 镜像这
一步把桶和前缀丢了, 不申报等于让来源闸失去判据(未申报是 fail-open, 见 contract 里
那段权衡)。**具结车道上这条是 fail-closed 的**: `--rights first_party` 而不申报来源
直接报错 —— 产出可训练字节却不留来源坐标等于只上了一把锁(docs/50 §6.1 R5 说的
"新车道从第一天起要求申报")。默认(不可训练)档沿用 fail-open。另: 具结车道下
`method-validation-only` 档的来源也会被拦, 因为那条车道的产物是要进训练的。

来源坐标的**成色**也留痕(`provenance.source_coordinate`): R2 模式是列桶列出来的事实,
本地镜像模式是操作者用 `--source-uri` 断言的 —— 复核的人有权分辨这两者。

## QC 选片门(`--qc-report` / `--qc-mode`)

`diogenes_wm.halo_qc` 出 session 级结论, 本模块只做装配 —— 两者的分工是刻意的:
判据的口径归 QC, 语料的取舍归适配器, 谁也不替对方做决定。

    halo_qc.load_report(path) -> QCReport   # .tier / .is_passed(sid) / .by_session

**档位由报告自带**: floor 档关掉 aim 三项判据并把 `combat.aim` 从必备通道里摘掉
(六通道降五, 与本模块默认 mask 掉该通道是同一件事的两面); strict 档全开。所以
`--qc-mode` 与报告的 tier **不一致时硬报错**, 不做换算 —— 拿 strict 的结论当 floor
用等于换了口径而不自知。报告里没有的 session 一律记为未过闸(不装配没体检的数据)。

`--qc-mode off`(默认, 且只在没给 `--qc-report` 时是默认)是唯一不需要 QC 的路径;
`halo_qc` 缺失时 `--qc-mode floor/strict` **报错而不是降级**。给了报告却又要
`off` 同样报错 —— 报告被静默忽略时, 统计里看不出"没过闸的 session 照常进了语料"。

## 断点续跑判的是两件事, 不是一件

`clip_id + manifest sha` 只回答"**同一批字节**吗"。合规戳、mask 口径、fps、game、
来源坐标都不是字节的属性 —— 只比字节就意味着**撤销一份具结、换一个 consent、关掉
mask 之后重跑会变成一次什么都没发生的空跑, 而统计报的是新口径**。所以续跑同时比
输出口径: 字节相同且口径相同 → 跳过; 字节相同但口径变了 → **重转**并计入
`chunks_reconverted_settings_changed`(重转会重写 p0.jsonl 与 meta.json, 所以是完整
的重盖, 不是只补 meta)。

## 全量的量级(为什么有 `--jobs`)

23.87 h 语料 ≈ 8,663 clip / 159 GiB 视频 + 3.5 万次小对象往返。本机实测串行
≈23 h(其中 2.6 h 纯粹是小对象延迟), 24 线程取小对象 24×、8 线程取视频 2.5×。
`--jobs N` 因此按批把该批要用的对象**并发预热到 cache**, 之后主循环照常串行跑 ——
预热是纯优化, 判定权仍在主循环, 预热猜错只影响带宽不影响任何结论。`--jobs 1`
(默认)完全不起线程池, 行为与预热不存在时逐字节相同。

## 布局

    <out>/<session_uuid>/<run_id>-<segment_id>/
        p0.mp4      符号链接到缓存/镜像里的源 mp4(不重编码、不复制)
        p0.jsonl    逐帧一行, 与视频帧 1:1
        meta.json   合规戳 + video 块(fps_num/fps_den 必填) + masked_actions + provenance

session 用 **R2 路径上的 session uuid**(服务端身份), 不是 `meta.session_id`(设备本地
id, 形如 `dev-1785830459723-0009`)。两者实测 1:1, 但 QC 报告、reclass 工具、对象存储
都以路径 uuid 为准, 训练侧的会话级切分也必须跟它对齐。设备侧 id 记在 provenance 里。
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
import re
import sys
import threading
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Mapping

from diogenes_phase0.contract import (
    assert_source_allowed, parse_source_uri, source_stamp,
)

MOUSE_MOVE = "mouse_move"

RIGHTS_CHOICES = ("third_party", "first_party")
QC_MODES = ("off", "floor", "strict")

DEFAULT_GAME_ID = "halo-infinite-steam-release-test"

#: 具结车道是混的(halo 8,663 / cod 6 / wolfenstein2 67 / wuchang 7 + 1 个无
#: target 的孤儿, docs/50 §6 R1–R3)。合规戳里的 `halo_partner_licensed` 是一份
#: **只覆盖 Halo 的具名授权**, 所以 `--game-id` 不是纯粹的选片参数 —— 它决定戳
#: 出来的授权声明是真话还是假话。换个 game_id 等于换了一份本模块并不知道的授权。
#: 授权成员与各作品的治理裁定统一收在 game_registry(静态 GOVERNANCE, 无 IO);
#: C-1 按作品成立, 覆盖 Halo 系(Infinite 为主、MCC/Reach 为辅, 见 c1-c4 确认函)。
from .game_registry import GOVERNANCE as _GAME_GOVERNANCE
from .game_registry import HALO_PARTNER_GAME_IDS

#: 授权边界必须跟着数据走: 三个月后翻到这批 chunk 的人只看得到 meta.json,
#: 而 `halo_partner_licensed` 单独一个标签读起来像"有授权, 可以发", 正好相反。
HALO_LICENSE_BOUNDARY = {
    "content": "Halo Infinite —— 微软合作授权内容, 不是无主素材",
    "authorization": "C-1",
    "covers": ["采集", "内部训练"],
    "not_covered": ["对外测试", "对外演示", "对外发布/分发",
                    "公开发布的 checkpoint 权重"],
    "pending": "C-2(对外测试)尚未落纸 —— 未取得之前 not_covered 全部适用",
    "evidence": "research/50 §4.1 · Halo 合作授权 C-1",
}

DEFAULT_FPS_NUM = 60
DEFAULT_FPS_DEN = 1
AIM_SEMANTIC = "combat.aim"
DEFAULT_MASK_ACTIONS = (AIM_SEMANTIC,)

#: `contract.pointer_clamp` —— converter 写 `pointerDx/Dy` 时的封顶值。这里只用它
#: 统计"clamp 影响了多少帧", 不参与任何写出的数值。
POINTER_CLAMP = 200

#: 转换必需的五件。第六件 `audio.pcm.zst` 不进 chunk(p0.mp4 不含音轨, 音频是独立的
#: PCM 流), 但它的哈希仍进 provenance —— 缺哪一件都要能查出来。
REQUIRED_LOGICAL = ("meta.json", "manifest.json", "frames.jsonl.zst",
                    "events.jsonl.zst", "video.mp4")

#: 文件名 → 逻辑名。`meta-<sha>.json` → `meta.json`
_LOGICAL_BY_STEM = {
    "meta": "meta.json",
    "manifest": "manifest.json",
    "frames": "frames.jsonl.zst",
    "events": "events.jsonl.zst",
    "video": "video.mp4",
    "audio": "audio.pcm.zst",
}
_NAME_RE = re.compile(r"^(meta|manifest|frames|events|video|audio)-([0-9a-f]{64})\.(.+)$")

#: `--qc-mode floor` 下豁免的失败项, **逐字对齐 `halo_qc.AIM_CHECKS`**:
#: `combat.aim` 本来就被 mask 掉了, 再拿"aim 占比过低 / toggle-zoom 签名不成立"
#: 去毙 session 等于用一个已经不进模型的通道决定语料存亡(research/50 §4.2)。
#:
#: ⚠ `channel_completeness` **不在**这里: halo_qc 的 floor 档并没有关掉它, 只是把
#: 必备通道从六个降为五个(`halo_qc.FLOOR.channels`)。把它一并豁免会让这条兜底路径
#: 比 floor **更松** —— 缺的可能是 `move.forward` 而不是 `combat.aim`, 从一个裸标签
#: 分不出来。`action_entropy` 同理不豁免: floor 报告本就不该列它, 列了说明那份报告
#: 不是 floor 口径, 拒掉是 fail-closed 的正确方向。
#: `tests/test_halo_adapter.py::test_waived_failures_track_halo_qc_aim_checks`
#: 把这条与 halo_qc 钉在一起, 两边不许漂移。
QC_WAIVED_FAILURES = frozenset({"aim_share", "toggle_zoom"})


class HaloAdapterError(RuntimeError):
    """转换器自身的硬错误(接口对不上、凭据缺失、语料无法解释)。"""


class UnreadablePayload(Exception):
    """某个 clip 的正文读不出来 —— 计数拒收, **不中断整批**。

    8,663 个 clip 的一次全量跑里, 一个坏对象让整趟白跑是不可接受的; 而"读不出来"
    与"哈希不符"在治理上是同一类结论(这份字节不能用), 都该落进 `rejected`。
    """


# --------------------------------------------------------------------------- #
# 合规戳
# --------------------------------------------------------------------------- #
def compliance_stamp(rights: str, consent_id: str | None, *,
                     game_id: str = DEFAULT_GAME_ID) -> dict:
    """两条车道的合规戳, 见模块 docstring。`production_certified` 两条都是 False。

    `game_id` 是承重参数而不是装饰: 授权声明按**作品**成立, 本模块只知道 Halo
    那一份(`HALO_PARTNER_GAME_IDS`)。
    """
    if rights not in RIGHTS_CHOICES:
        raise ValueError(f"unknown rights {rights!r}; expected {RIGHTS_CHOICES}")
    licensed = game_id in HALO_PARTNER_GAME_IDS
    if _GAME_GOVERNANCE.get(game_id, ("", ""))[0] == "ungranted_blocked":
        raise ValueError(
            f"game_id {game_id!r} 是 ungranted 通道(采集本身未授权)——两条车道都"
            "拒绝, 这批数据不该被任何取数工具消费。")
    if rights == "third_party":
        if consent_id:
            raise ValueError(
                "third_party 车道不接受 consent-id —— 源对象的 meta 至今写的是 "
                "non_trainable=true, 默认档就是逐字抄它; 要走具结车道请显式传 "
                "--rights first_party")
        return {
            "rights": "local_debug_non_trainable",
            "first_party": False,
            "consent_id": None,
            "non_trainable": True,
            "is_trainable": False,
            "production_certified": False,
            # ⚠ 源 schema 里**没有** cutscene 字段(实测这批六件套均无), 所以这一项
            # 是断言而不是抄录 —— 这条产线录的是连续 gameplay, 没有过场语义。
            # 黑屏/菜单段的识别是 docs/50 §6 明记的未覆盖项(QC 不解码视频),
            # 源侧的原值另记在 provenance.source_declares_cutscene。
            "is_cutscene": False,
            "tags": [("halo_partner_licensed" if licensed
                      else "license_unknown_non_halo_title"),
                     "method_validation_only", "non_trainable", "no_publish"],
        }
    if not licensed:
        cls, note = _GAME_GOVERNANCE.get(game_id, ("unregistered", "不在注册表"))
        by_class = {
            "microsoft_family": (
                "作品归微软但 C-1 文本只署 Halo —— 扩展要新文书(参照 doc 29 对齐"
                "补记的'微软名下多款游戏'路径), 或走该作品自己的 per-title 具结"
                "(COD 先例: cod_adapter)。"),
            "third_party": "第三方作品, 无任何已落纸授权判定 —— 先谈授权再谈取数。",
            "unregistered": "不在 dev catalog/注册表 —— 先进 catalog + GOVERNANCE。",
        }
        raise ValueError(
            f"--game-id {game_id!r} 不在 Halo 合作授权覆盖的作品内 "
            f"({sorted(HALO_PARTNER_GAME_IDS)}); 具结车道会给每个 chunk 盖 "
            f"halo_partner_licensed + is_trainable=true, 而那份授权只覆盖 Halo。"
            f"[{cls}: {note}] " + by_class.get(cls, ""))
    if not (consent_id or "").strip():
        raise ValueError(
            "--rights first_party 必须同时给 --consent-id(owner 具结 ID)。"
            "门是 first_party ∧ consent_id ∧ ¬cutscene ∧ is_trainable 四项与, "
            "只翻一个会让人以为改了其实没改。")
    return {
        "rights": "first_party",
        "first_party": True,
        "consent_id": consent_id.strip(),
        "non_trainable": False,
        "is_trainable": True,
        # 产品认证位: 具结说的是"验证用、不发布", 那不是产品认证
        "production_certified": False,
        # 断言而非抄录, 理由同上(默认档那段注释)
        "is_cutscene": False,
        # halo_partner_licensed: 微软合作授权内容。C-1 覆盖采集与内部训练;
        # 对外测试/演示/发布需要 C-2, 未取得 —— 所以 no_publish 依然成立。
        # c2_not_obtained / no_external_use 把那条**待办**也写进数据: 只写
        # no_publish 会被读成"不发就行", 而 C-2 挡的是一切对外使用。
        "tags": ["owner_attested_validation", "no_publish", "no_external_use",
                 "halo_partner_licensed", "c2_not_obtained",
                 "no_product_training", "trainable"],
        "license_boundary": HALO_LICENSE_BOUNDARY,
    }


# --------------------------------------------------------------------------- #
# 基础工具
# --------------------------------------------------------------------------- #
def _sha256_bytes(blob: bytes) -> str:
    return hashlib.sha256(blob).hexdigest()


def _sha256_file(path: Path, chunk: int = 1 << 20) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(chunk):
            h.update(block)
    return h.hexdigest()


def _strip_sha_prefix(value: str | None) -> str | None:
    """manifest / meta 里写的是 `"sha256:<hex>"`, 归一成裸 hex。"""
    if not isinstance(value, str):
        return None
    return value[7:] if value.startswith("sha256:") else value


def unzstd(blob: bytes) -> bytes:
    """流式解压。

    直接 `ZstdDecompressor().decompress()` 会在这批语料上炸 —— 帧头没写
    content size。空 events 是 9 字节的空 zstd 帧, 解出来就是 b""。
    """
    if not blob:
        return b""
    try:
        import zstandard as zstd
    except ImportError as e:  # pragma: no cover - 依赖缺失路径
        raise HaloAdapterError(
            "需要 zstandard 才能读 frames/events(pip install '.[data]')") from e
    try:
        return zstd.ZstdDecompressor().stream_reader(io.BytesIO(blob)).read()
    except zstd.ZstdError as e:
        # 依赖缺失是**本机**的问题(硬错), 解压失败是**这份字节**的问题(拒收)
        raise UnreadablePayload(f"zstd 解压失败: {e}") from e


def _jsonl(blob: bytes) -> list[dict]:
    """JSONL → list[dict]。坏字节抛 `UnreadablePayload`(拒收), 不中断整批。"""
    try:
        text = blob.decode("utf-8")
    except UnicodeDecodeError as e:
        raise UnreadablePayload(f"正文不是 UTF-8: {e}") from e
    out: list[dict] = []
    for n, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError as e:
            raise UnreadablePayload(f"第 {n} 行不是 JSON: {e}") from e
    return out


# --------------------------------------------------------------------------- #
# 指针重算
# --------------------------------------------------------------------------- #
def _need(row: Mapping, name: str, what: str):
    """取一个**承重**字段, 缺了就按"这份字节读不出来"拒收(而不是 KeyError 中断整批)。"""
    try:
        return row[name]
    except (KeyError, TypeError) as e:
        raise UnreadablePayload(f"{what} 缺字段 {name!r}") from e


def raw_pointer_by_frame(
        events: Iterable[dict],
        frames: Iterable[dict]) -> tuple[dict[int, tuple[int, int]], int, int]:
    """未截断的逐帧指针位移 + 跳过的绝对坐标事件数 + **落在帧窗口之外**的移动数.

    按帧 `[output_qpc_start, output_qpc_end)` **半开**区间求和该区间内所有
    `mouse_move` 的 dx/dy。实测帧窗口在 clip 内首尾相接、无缝隙
    (`output_qpc_end[i] == output_qpc_start[i+1]`), 事件按 qpc 单调, 所以一次双指针
    扫过即可, 不必对每帧重扫事件表。

    "无缝隙"是**承重前提**, 所以它被数出来而不是写在注释里: 落在缝隙里的
    mouse_move 会从每一帧的和里消失, 而 `frames.jsonl` 自带的摘要是同一套窗口切出来
    的, 两边会一起丢 —— 对账兜不住它。第三个返回值非零即表示前提破了, 重算出来的
    指针标签是有损的。(生产者 `projection.rs` 目前用 `ensure!` 强制了连续性, 所以
    这是给未来 schema/归一化变更留的判据。)

    `absolute=true` 的 mouse_move 携带的是**绝对坐标**而非增量, 把它加进和里是纯粹
    的错误 —— 跳过并计数(实测这批语料 0 命中, 但不能默认它永远是 0)。
    """
    moves: list[tuple[int, int, int]] = []
    skipped_absolute = 0
    for e in events:
        if e.get("type") != MOUSE_MOVE:
            continue
        if e.get("absolute"):
            skipped_absolute += 1
            continue
        moves.append((int(_need(e, "qpc", "events.jsonl")),
                      int(e.get("dx") or 0), int(e.get("dy") or 0)))
    moves.sort(key=lambda m: m[0])

    out: dict[int, tuple[int, int]] = {}
    i = 0
    orphaned = 0
    prev_hi = None
    for fr in sorted(frames, key=lambda r: int(_need(r, "frame", "frames.jsonl"))):
        lo = _need(fr, "output_qpc_start", "frames.jsonl")
        hi = _need(fr, "output_qpc_end", "frames.jsonl")
        while i < len(moves) and moves[i][0] < lo:
            # 落在 [上一帧的 end, 这一帧的 start) 里的事件 = 没有任何帧收下它。
            # 首帧窗口之前的输入不算(录制早于第一帧输出, 是正常的), 上一帧已经
            # 收走的事件也不算(内层 j 循环不推进 i, 所以这里会重走它们)。
            if prev_hi is not None and moves[i][0] >= prev_hi:
                orphaned += 1
            i += 1
        prev_hi = hi
        j, sx, sy = i, 0, 0
        while j < len(moves) and moves[j][0] < hi:
            sx += moves[j][1]
            sy += moves[j][2]
            j += 1
        out[int(fr["frame"])] = (sx, sy)
    return out, skipped_absolute, orphaned


def _pointer_digest(frames: Iterable[dict]) -> tuple[dict[int, tuple[int, int]], int]:
    """`frames.jsonl` 自带的逐帧事件摘要(`events: [{type,dx,dy}]`)求和.

    它没有 qpc, 所以**不是**权威来源 —— 权威来源是上面的半开区间重算。但它是一份
    独立记录, 拿来对账等于每次转换都重跑一遍"帧-事件对齐"的验证。
    """
    out: dict[int, tuple[int, int]] = {}
    total = 0
    for fr in frames:
        sx = sy = 0
        for e in fr.get("events") or []:
            if e.get("type") == MOUSE_MOVE:
                total += 1
                sx += int(e.get("dx") or 0)
                sy += int(e.get("dy") or 0)
        out[int(fr["frame"])] = (sx, sy)
    return out, total


# --------------------------------------------------------------------------- #
# 源: 本地镜像 / R2
# --------------------------------------------------------------------------- #
@dataclass
class ClipRef:
    """一个 clip 的定位信息(尚未读取内容)。"""
    session_id: str          # R2 路径上的 session uuid(服务端身份)
    run_uuid: str
    clip_id: str
    #: 逻辑名 → (定位符, 文件名内嵌 sha256, 字节数或 None)
    files: dict[str, tuple[str, str, int | None]] = field(default_factory=dict)
    #: 同一个逻辑名在这个 clip 前缀下出现了**两代不同内容**(内容寻址 = 重传会
    #: 留下两份)。绑哪一代取决于列举顺序 —— 那不能作为取舍依据, 所以整 clip 拒收。
    ambiguous: set[str] = field(default_factory=set)

    @property
    def key(self) -> str:
        return f"{self.session_id}/{self.run_uuid}/{self.clip_id}"


def _clip_parts(parts: list[str]) -> tuple[str, str, str] | None:
    """从路径分段里认出 `<session>/runs/<run>/clips/<clip>/<file>`。

    对 `sessions/` 这一层是否存在不敏感 —— rclone/aws s3 sync 下来的本地镜像通常
    以 session uuid 为根。
    """
    if len(parts) < 6:
        return None
    if parts[-3] != "clips" or parts[-5] != "runs":
        return None
    return parts[-6], parts[-4], parts[-2]


def _index_clips(entries: Iterable[tuple[list[str], str, int | None]]) -> dict[str, ClipRef]:
    """(路径分段, 定位符, 字节数) 流 → clip 索引。"""
    clips: dict[str, ClipRef] = {}
    for parts, locator, size in entries:
        ids = _clip_parts(parts)
        if ids is None:
            continue
        m = _NAME_RE.match(parts[-1])
        if m is None:
            continue
        session_id, run_uuid, clip_id = ids
        ref = clips.setdefault(f"{session_id}/{run_uuid}/{clip_id}",
                              ClipRef(session_id, run_uuid, clip_id))
        logical = _LOGICAL_BY_STEM[m.group(1)]
        prior = ref.files.get(logical)
        if prior is not None and prior[1] != m.group(2):
            # 内容寻址意味着一次重传会在同一个 clip 前缀下留下**两代**
            # (frames-<a>… 与 frames-<b>…), 而 research/50 §7 记了这条产线上
            # 真实发生过的重复上传。"最后一个赢"会让绑哪一代取决于 rglob /
            # list_objects_v2 的列举顺序 —— 同一份语料能给出两种结论, 所以这里
            # 只记异常、保留先到的那一代, 由 convert() 整 clip 拒收。
            ref.ambiguous.add(logical)
            continue
        ref.files[logical] = (locator, m.group(2), size)
    return clips


class LocalSource:
    """本地镜像。`materialize` 直接返回原路径, 不复制。"""

    kind = "local"

    def __init__(self, root: Path):
        self.root = Path(root).resolve()
        if not self.root.is_dir():
            raise HaloAdapterError(f"--src 不是目录: {self.root}")

    def describe(self) -> str:
        return str(self.root)

    def iter_clips(self) -> dict[str, ClipRef]:
        entries = []
        for path in self.root.rglob("*"):
            if not path.is_file():
                continue
            rel = path.relative_to(self.root).parts
            entries.append((list(rel), str(path), path.stat().st_size))
        return _index_clips(entries)

    def materialize(self, clip: ClipRef, logical: str, cache: Path | None) -> tuple[Path, bool]:
        locator, _sha, _size = clip.files[logical]
        return Path(locator), False

    def read(self, clip: ClipRef, logical: str, cache: Path | None) -> bytes:
        path, _ = self.materialize(clip, logical, cache)
        return path.read_bytes()

    def evict(self, clip: ClipRef, logical: str, cache: Path | None) -> bool:
        """本地镜像**永不删**: 那是用户的源数据, 不是我们的缓存副本。"""
        return False


class R2Source:
    """R2(S3 兼容)。所有内容先落到 `--cache` 再转换, 支持断点续跑。"""

    kind = "r2"

    def __init__(self, bucket: str, prefix: str, *, credentials_dir: Path | None = None):
        self.bucket = bucket
        self.prefix = prefix
        self._creds_dir = Path(credentials_dir) if credentials_dir else Path("/tmp")
        self._client = None
        # `--jobs > 1` 时多个线程会同时第一次碰 .client
        self._client_lock = threading.Lock()

    def describe(self) -> str:
        return f"r2://{self.bucket}/{self.prefix}"

    def _cred(self, name: str, env: str) -> str:
        value = os.environ.get(env)
        if value:
            return value.strip()
        path = self._creds_dir / f"r2-training-{name}.txt"
        if not path.exists():
            raise HaloAdapterError(
                f"缺 R2 凭据: 既没有环境变量 {env}, 也没有 {path}")
        return path.read_text().strip()

    @property
    def client(self):
        with self._client_lock:
            if self._client is None:
                try:
                    import boto3
                except ImportError as e:  # pragma: no cover - 依赖缺失路径
                    raise HaloAdapterError(
                        "R2 模式需要 boto3(pip install '.[data]');"
                        " 本地镜像模式不需要") from e
                account = self._cred("account-id", "R2_ACCOUNT_ID")
                endpoint = os.environ.get(
                    "R2_ENDPOINT_URL",
                    f"https://{account}.r2.cloudflarestorage.com")
                self._client = boto3.client(
                    "s3", endpoint_url=endpoint,
                    aws_access_key_id=self._cred("access-key-id",
                                                 "R2_ACCESS_KEY_ID"),
                    aws_secret_access_key=self._cred("secret-access-key",
                                                     "R2_SECRET_ACCESS_KEY"),
                    region_name="auto")
        return self._client

    def iter_clips(self) -> dict[str, ClipRef]:
        entries, token = [], None
        while True:
            kw = dict(Bucket=self.bucket, Prefix=self.prefix, MaxKeys=1000)
            if token:
                kw["ContinuationToken"] = token
            resp = self.client.list_objects_v2(**kw)
            for obj in resp.get("Contents", []):
                entries.append((obj["Key"].split("/"), obj["Key"], obj["Size"]))
            if not resp.get("IsTruncated"):
                break
            token = resp["NextContinuationToken"]
        return _index_clips(entries)

    def _cache_path(self, clip: ClipRef, logical: str, cache: Path) -> Path:
        key, _sha, _size = clip.files[logical]
        return (cache / clip.session_id / clip.run_uuid / clip.clip_id
                / key.split("/")[-1])

    def materialize(self, clip: ClipRef, logical: str,
                    cache: Path | None) -> tuple[Path, bool]:
        if cache is None:
            raise HaloAdapterError("R2 模式必须给 --cache(先下载再转换)")
        key, sha, size = clip.files[logical]
        dest = self._cache_path(clip, logical, cache)
        if dest.exists() and (size is None or dest.stat().st_size == size):
            # 命中缓存。内容寻址的文件名 + 下面按 --verify 策略的重算哈希共同兜底,
            # 断点续跑时不必把整个视频重新哈希一遍。
            return dest, False
        dest.parent.mkdir(parents=True, exist_ok=True)
        # 临时名带 pid+线程 id: `--jobs>1` 与"两个进程共用一个 cache"都不该互相
        # 覆盖对方写到一半的文件
        tmp = dest.with_name(f"{dest.name}.{os.getpid()}-{threading.get_ident()}.part")
        try:
            self.client.download_file(self.bucket, key, str(tmp))
            tmp.replace(dest)
        finally:
            if tmp.exists():
                tmp.unlink()
        return dest, True

    def read(self, clip: ClipRef, logical: str, cache: Path | None) -> bytes:
        if cache is not None:
            path, _ = self.materialize(clip, logical, cache)
            return path.read_bytes()
        key, _sha, _size = clip.files[logical]
        return self.client.get_object(Bucket=self.bucket, Key=key)["Body"].read()

    def evict(self, clip: ClipRef, logical: str, cache: Path | None) -> bool:
        """删掉缓存里这一份, 让下一次读重新下载。

        缓存命中的判据只有文件大小, 所以一份**大小相同但内容坏掉**的缓存文件会让
        该 clip 在此后**每一次**跑里都被静默丢掉, 唯一的痕迹是聚合计数 +1。
        校验不过就把它清掉, 是把"坏缓存"从永久损失降级成一次重下。
        """
        if cache is None:
            return False
        path = self._cache_path(clip, logical, Path(cache))
        if path.exists():
            path.unlink()
            return True
        return False


def build_source(src: str, *, credentials_dir: Path | None = None):
    """`r2://<bucket>/<prefix>` 或本地路径。"""
    if src.startswith("r2://") or src.startswith("s3://"):
        rest = src.split("://", 1)[1]
        bucket, _, prefix = rest.partition("/")
        if not bucket:
            raise HaloAdapterError(f"--src 缺 bucket: {src!r}")
        return R2Source(bucket, prefix, credentials_dir=credentials_dir)
    return LocalSource(Path(src))


# --------------------------------------------------------------------------- #
# QC 选片门
# --------------------------------------------------------------------------- #
_MISSING = object()


def _field(entry, name):
    if isinstance(entry, Mapping):
        return entry.get(name, _MISSING)
    return getattr(entry, name, _MISSING)


def _qc_entries(report) -> list[tuple[str, object]]:
    """把 halo_qc 的报告归一成 [(session_id, entry)]。"""
    for attr in ("sessions", "by_session", "results"):
        inner = _field(report, attr)
        if inner is not _MISSING and inner is not None:
            report = inner
            break
    if isinstance(report, Mapping):
        return [(str(k), v) for k, v in report.items()]
    if isinstance(report, (list, tuple)):
        out = []
        for entry in report:
            sid = _field(entry, "session_id")
            if sid is _MISSING:
                sid = _field(entry, "session")
            if sid is _MISSING:
                raise HaloAdapterError(
                    "QC 报告的条目里找不到 session_id / session 字段, 无法对齐 "
                    "session —— 拒绝猜。")
            out.append((str(sid), entry))
        return out
    raise HaloAdapterError(
        f"看不懂的 QC 报告结构 {type(report).__name__}; 期望 mapping 或 "
        "SessionQC 序列。")


def _qc_verdict(entry, mode: str) -> bool:
    """判定一个 session 是否过闸。找不到判据就报错, 绝不默认放行。

    这是**兜底**路径 —— `halo_qc.QCReport` 走的是上面的原生 `is_passed()`。它存在
    是为了让适配器不被一个具体实现锁死(比如手写的选片名单)。豁免表
    `QC_WAIVED_FAILURES` 只在这条兜底路径上生效, 且只在 floor 档: `halo_qc` 的
    floor 档本来就不把 aim 两项算进 `failed_checks`, 不需要这里再豁免一次。
    **手写报告必须用 halo_qc 的判据 id**(`halo_qc.CHECK_ORDER`) —— 自造词表会让
    豁免要么落空要么过头, 而两种失效在裸标签上都看不出来。
    """
    names = (("passes_floor", "floor_pass", "passed_floor", "floor")
             if mode == "floor" else
             ("passes_strict", "strict_pass", "passed_strict", "strict"))
    for name in names:
        value = _field(entry, name)
        if isinstance(value, bool):
            return value
    # 档位一致性已在 load_qc_gate 里核过, 所以报告自己的 `passed` 就是这一档的结论
    for name in ("passed", "ok", "is_pass"):
        value = _field(entry, name)
        if isinstance(value, bool):
            return value
    for name in ("failures", "fails", "failed_checks", "violations"):
        value = _field(entry, name)
        if isinstance(value, (list, tuple, set, frozenset)):
            fails = {str(x) for x in value}
            if mode == "floor":
                fails -= QC_WAIVED_FAILURES
            return not fails
    raise HaloAdapterError(
        f"QC 条目里找不到 {mode} 档的判据(试过 {names}、passed/ok 与 "
        "failures/failed_checks 列表)。接口没对上就报错, 不静默放行。")


def _qc_gate_from_report(report, mode: str) -> dict[str, bool]:
    """报告 → {session_id: 是否过闸}。

    **档位由报告自带**(`halo_qc` 的 tier 决定哪些判据参与判定: floor 关掉 aim 三项
    并把 `combat.aim` 从必备通道里摘掉, 即六通道降五)。所以这里不"换算"档位, 只做
    一致性核对 —— 拿 strict 的结论当 floor 用(或反过来)等于换了口径而不自知。
    """
    tier = _field(report, "tier")
    if isinstance(tier, str) and tier and tier != mode:
        raise HaloAdapterError(
            f"QC 报告是 {tier!r} 档跑出来的, 而 --qc-mode 要的是 {mode!r}。"
            "档位决定哪些判据参与判定, 两者不能互相换算 —— 要么用 "
            f"--qc-mode {tier}, 要么用 {mode} 档重跑 halo_qc。")
    if not isinstance(tier, str) or not tier:
        # halo_qc 的报告一定带 tier; 没有 tier 的是手写/第三方报告 —— 那就核对不了
        # 口径, 只能按 --qc-mode 的字面值判定。这件事要说出来, 不能默默发生。
        print(f"⚠ QC 报告没有声明 tier, 无法核对档位口径 —— 按 --qc-mode "
              f"{mode!r} 的字面值判定。halo_qc 导出的报告一定带 tier。",
              file=sys.stderr)
    # halo_qc.QCReport 的原生接口: 未体检的 session 默认 False(不装配没体检的数据)
    is_passed = _field(report, "is_passed")
    by_session = _field(report, "by_session")
    if callable(is_passed) and isinstance(by_session, Mapping):
        return {sid: bool(is_passed(sid)) for sid in by_session}
    return {sid: _qc_verdict(entry, mode) for sid, entry in _qc_entries(report)}


def load_qc_gate(report_path: Path | None, mode: str) -> dict[str, bool] | None:
    """→ {session_id: 是否过闸}; `off` 档返回 None。"""
    if mode not in QC_MODES:
        raise ValueError(f"unknown qc mode {mode!r}; expected {QC_MODES}")
    if mode == "off":
        if report_path is not None:
            raise HaloAdapterError(
                "给了 QC 报告却又要 qc_mode='off' —— 报告会被完全忽略, 没过闸的 "
                "session 会照常装配, 而统计里看不出这件事(qc_mode 只会写着 off)。"
                "要体检就给 floor/strict, 不体检就别传报告。")
        return None
    if report_path is None:
        raise HaloAdapterError(f"--qc-mode {mode} 必须同时给 --qc-report")
    try:
        from . import halo_qc  # noqa: PLC0415 - 该模块可能尚未落地
    except ImportError as e:
        raise HaloAdapterError(
            f"--qc-mode {mode} 需要 diogenes_wm.halo_qc, 但它导入失败({e})。"
            "**不做静默降级** —— 要么等该模块合入, 要么显式 --qc-mode off 并自己"
            "为选片负责。") from e
    loader = getattr(halo_qc, "load_report", None)
    if loader is None:
        raise HaloAdapterError(
            "diogenes_wm.halo_qc 没有导出 load_report(path); 接口没对上, 拒绝猜。")
    return _qc_gate_from_report(loader(Path(report_path)), mode)


# --------------------------------------------------------------------------- #
# 逐 clip 转换
# --------------------------------------------------------------------------- #
def _chunk_meta(*, clip: ClipRef, meta: dict, n_frames: int, chunk_name: str,
                stamp: dict, masked: dict, manifest_sha: str,
                fps_num: int, fps_den: int, empty_events: bool,
                source_bucket: str | None, source_prefix: str,
                source_observed: bool) -> dict:
    catalog = meta.get("catalog") or {}
    mapping = meta.get("mapping") or catalog.get("mapping") or {}
    target = meta.get("target") or {}
    audio = meta.get("audio") or {}
    tags = list(stamp["tags"])
    if AIM_SEMANTIC not in (masked.get("semantics") or ()):
        # mask 被显式关掉时也要留痕: 只在 masked_actions 里写空列表, 读起来与
        # "这批数据本来就没有 aim 语义"无法区分, 而 docs/50 §4.2 的裁决是必须 mask。
        tags.append("unmasked_combat_aim")
    if empty_events:
        # 真实的操作空窗(实测 128 个 clip): frames 仍完整, 但这一段没有任何输入事件。
        # research/50 的裁决是**降权不剔除** —— 标签跟着数据走, 下游据此降权。
        tags.append("empty_events")
    out = {
        # 身份**只从落盘的目录名派生**: 撞名时目录会加 clip uuid 后缀, 而下游
        # (tokenizer_probe / ab_p0 / eval.report / readiness / contract 的每一条
        # 报错)全部按 sample_key 与 chunk 对齐 —— 目录分开了而身份还撞着, 等于没分。
        "sample_key": f"{clip.session_id}_{chunk_name}",
        "session": clip.session_id,
        "chunk": chunk_name,
        "game": target.get("game_id"),
        "platform": "pc",
        "schema_version": "dm-1",
        "key_schema": "dm-1 mapping (per-game physical vocab)",
        "annotation_source": "raw_input_recorded",
        "video": {
            "file": "p0.mp4",
            "codec": "h264",
            "width": meta.get("width"),
            "height": meta.get("height"),
            # fps 是承重字段: 契约拒绝"没声明 fps"的 chunk 而不是默认 30
            # (docs/50 P-4/P-8), 且必须给有理数真值 —— 59.94 与 60.00 都会被
            # 四舍五入成同一个整数 60, 那正是混合 fps 门要拦的东西
            "fps": fps_num / fps_den,
            "fps_num": fps_num,
            "fps_den": fps_den,
            "frames": n_frames,
            # p0.mp4 不含音轨(音频是独立的 PCM 流, 没有搬进 chunk)
            "audio": False,
        },
        "masked_actions": masked,
        "provenance": {
            "source": "R2 diogenes-model-training raw/v1/attested-trainable "
                      "(dataminer-api-production)",
            "lane": "attested-trainable",
            "clip_id": clip.clip_id,
            "run_uuid": clip.run_uuid,
            "run_id": meta.get("run_id"),
            "device_session_id": meta.get("session_id"),
            "segment_index": meta.get("segment_index"),
            "session_frame_start": meta.get("session_frame_start"),
            "run_state": meta.get("state"),
            "input_status": meta.get("input_status"),
            "source_schema_version": meta.get("schema_version"),
            "source_non_trainable": meta.get("non_trainable"),
            "source_production_certified": meta.get("production_certified"),
            # 源 schema 里没有 cutscene 字段(实测), 所以这里几乎必然是 null ——
            # 合规戳里的 is_cutscene=False 是断言而不是抄录, 这一格就是那句话的证据
            "source_declares_cutscene": meta.get("is_cutscene"),
            "catalog_id": catalog.get("catalog_id"),
            "catalog_version": catalog.get("catalog_version"),
            "catalog_mode": catalog.get("mode"),
            # 哈希一律归一成裸 hex: 同一个 provenance 块里两种写法, 会让任何按
            # mapping_content_hash / executable_sha256 去 join 目录的人先猜格式
            "catalog_content_hash": _strip_sha_prefix(
                catalog.get("catalog_content_hash")),
            "mapping_id": mapping.get("id"),
            "mapping_version": mapping.get("version"),
            "mapping_content_hash": _strip_sha_prefix(mapping.get("content_hash")),
            "target_id": target.get("target_id"),
            "executable_sha256": _strip_sha_prefix(target.get("executable_sha256")),
            "manifest_sha256": manifest_sha,
            # 来源坐标的成色: R2 模式是列桶列出来的事实, 本地镜像模式是操作者用
            # --source-uri 断言的(镜像那一步把桶和前缀丢了)。两者都写进
            # source.bucket/prefix 供 SOURCE_RULES 判定, 但复核的人有权分辨。
            "source_coordinate": ("observed (r2 listing)" if source_observed
                                  else "asserted (--source-uri, local mirror)"),
            "video_sha256": _strip_sha_prefix((meta.get("video") or {}).get("sha256")),
            "frames_sha256": _strip_sha_prefix((meta.get("frames") or {}).get("sha256")),
            "events_sha256": _strip_sha_prefix((meta.get("events") or {}).get("sha256")),
            "audio_sha256": _strip_sha_prefix(audio.get("sha256")),
            "audio_available": bool(clip.files.get("audio.pcm.zst")),
            "qpc_start": meta.get("qpc_start"),
            "qpc_end": meta.get("qpc_end"),
            "qpc_frequency": audio.get("qpc_frequency"),
            "pointer_clamp": (meta.get("contract") or {}).get("pointer_clamp"),
            "health": meta.get("health"),
        },
        **stamp,
    }
    out["tags"] = tags
    if source_bucket:
        # 来源坐标是搬运时定下的、适配器改不了的事实 —— contract.SOURCE_RULES 那把
        # 反向锁读的就是它(docs/50 §6.1 R5/P2)。只用 source_stamp() 这一个写法。
        out.update(source_stamp(source_bucket, source_prefix))
    return out


def _write_actions(path: Path, frames: list[dict], raw: dict[int, tuple[int, int]],
                   mask: frozenset[str], stats: Counter,
                   masked_counts: Counter) -> None:
    with open(path, "w") as f:
        for r in frames:
            idx = int(r["frame"])
            rdx, rdy = raw.get(idx, (0, 0))
            if abs(rdx) > POINTER_CLAMP or abs(rdy) > POINTER_CLAMP:
                stats["frames_beyond_clamp"] += 1
            stats["max_abs_raw_pointer"] = max(stats["max_abs_raw_pointer"],
                                               abs(rdx), abs(rdy))
            actions = list(r.get("actions") or [])
            dropped = [a for a in actions if a in mask]
            row = {
                "frame": idx,
                "gframe": r.get("session_frame"),
                "video_t_ms": int(r.get("video_t_us") or 0) // 1000,
                "keys": list(r.get("keys") or []),
                "buttons": list(r.get("buttons") or []),
                "actions": [a for a in actions if a not in mask],
                # converter 已 clamp 到 ±200, 是 pointerBins 的来源
                "pointerDx": r.get("pointerDx", 0),
                "pointerDy": r.get("pointerDy", 0),
                # 从 events 半开区间重算的未截断真值
                "pointerRawDx": rdx,
                "pointerRawDy": rdy,
                "pointerBins": list(r.get("pointerBins") or [0, 0]),
                "duplicate": bool(r.get("duplicate", False)),
                "src": r.get("src", "rec"),
            }
            if dropped:
                # 逐帧留痕, mask 可逆
                row["actions_masked"] = dropped
                stats["masked_action_frames"] += 1
                masked_counts.update(dropped)
            for health in ("queue_drop", "source_superseded"):
                if r.get(health):
                    row[health] = r[health]
            f.write(json.dumps(row) + "\n")


# --------------------------------------------------------------------------- #
# 主流程
# --------------------------------------------------------------------------- #
def _segment_id(segment_index) -> str:
    """`segment_index` → 目录用的段号。非整数(不该出现)时原样字符串化并留痕。"""
    return (f"{int(segment_index):05d}" if isinstance(segment_index, int)
            else str(segment_index))


def _video_checked(source, clip: ClipRef, cache: Path | None, budget: int,
                   stats: Counter, hash_detail: Counter):
    """落地 video 并按预算核哈希 → (路径, 是否重算过哈希, 剩余预算); None = 拒收.

    刚下载的一律验(相对下载成本近乎免费, 挡的是传输损坏); 命中缓存/本地镜像的按
    `--verify` 预算抽验。校验不过先清缓存重下一次 —— 见 `_read_checked`。
    """
    for attempt in (0, 1):
        path, fresh = source.materialize(clip, "video.mp4", cache)
        if not (fresh or budget > 0):
            return path, False, budget
        if _sha256_file(path) == clip.files["video.mp4"][1]:
            return path, True, budget - (0 if fresh else 1)
        if attempt == 0 and source.evict(clip, "video.mp4", cache):
            stats["cache_evicted"] += 1
            continue
        hash_detail["video.mp4"] += 1
        return None
    return None


#: meta 自己也声明了各文件的 sha256(与 manifest、与文件名共三份)。第三份不核对
#: 就等于把一个没人看过的哈希抄进 chunk 的 provenance —— 而那正是复核的人回头拿去
#: 跟 R2 对账的字段。
_META_SHA_BLOCKS = {"video.mp4": "video", "frames.jsonl.zst": "frames",
                    "events.jsonl.zst": "events", "audio.pcm.zst": "audio"}


def _pool_materialize(source, items: list[tuple[ClipRef, str]],
                      cache: Path | None, jobs: int) -> int:
    """并发把 (clip, 逻辑名) 落到 cache, 返回失败个数.

    **纯优化**: 判定权仍在主循环 —— 预热失败只记数, 主循环会照常自己去读那份对象
    并按既有路径处理(下载失败在那里是硬错, 在这里不是)。
    """
    if not items or jobs <= 1:
        return 0

    def one(pair) -> bool:
        clip, logical = pair
        try:
            source.materialize(clip, logical, cache)
            return True
        except Exception:  # noqa: BLE001 - 预热失败不许中断转换
            return False

    with ThreadPoolExecutor(max_workers=jobs) as ex:
        return sum(1 for ok in ex.map(one, items) if not ok)


def _batches(seq: list, size: int):
    for i in range(0, len(seq), size):
        yield seq[i:i + size]


def convert(src: str | Path, out: Path, *,
            cache: Path | None = None,
            game_id: str = DEFAULT_GAME_ID,
            fps_num: int = DEFAULT_FPS_NUM,
            fps_den: int = DEFAULT_FPS_DEN,
            mask_actions: Iterable[str] = DEFAULT_MASK_ACTIONS,
            verify: int = 8,
            rights: str = "third_party",
            consent_id: str | None = None,
            qc_report: Path | None = None,
            qc_mode: str = "off",
            sessions: Iterable[str] | None = None,
            max_clips_per_session: int | None = None,
            limit: int | None = None,
            max_examined: int | None = None,
            drop_empty_events: bool = False,
            strict_pointer_digest: bool = False,
            overwrite: bool = False,
            credentials_dir: Path | None = None,
            source_uri: str | None = None,
            jobs: int = 1,
            prefetch_batch: int = 32,
            rejects_jsonl: Path | None = None,
            reject_log_cap: int = 2000,
            progress: bool = False,
            min_free_gib: float = 2.0) -> dict:
    """R2/本地镜像的 Halo clip → chunk 契约目录。返回统计 dict。"""
    stamp = compliance_stamp(rights, consent_id, game_id=game_id)
    mask = frozenset(mask_actions or ())
    source = build_source(str(src), credentials_dir=credentials_dir)
    if isinstance(source, R2Source) and cache is None:
        raise HaloAdapterError("R2 模式必须给 --cache: 先下载到本地再转换")

    # 来源坐标: R2 模式从 --src 直接读; 本地镜像要 --source-uri 申报它是从哪搬来的
    # (镜像这一步把桶/前缀丢了, 不申报就等于让来源闸失去判据)。
    if isinstance(source, R2Source):
        base_bucket, base_prefix = source.bucket, source.prefix
    else:
        base_bucket, base_prefix = parse_source_uri(source_uri) if source_uri \
            else (None, "")
    if not base_bucket and rights == "first_party":
        # 具结车道产出的是**可训练**字节, 而两把锁里的第二把(来源闸)读的就是来源
        # 坐标。未申报是 fail-open(contract 里那段权衡), 对不可训练的默认档可以
        # 接受, 对这条车道不行 —— 这正是 docs/50 §6.1 R5 说的"新车道从第一天起
        # 要求申报"。
        raise HaloAdapterError(
            "具结车道(--rights first_party)必须申报来源: 本地镜像模式请给 "
            "--source-uri r2://<bucket>/<prefix>。四项与是**自陈**的(适配器自己写"
            "的布尔, 可以写错), 来源坐标是搬运时定下的、适配器改不了的事实 —— "
            "产出可训练 chunk 却不留来源坐标, 等于只上了一把锁。")
    if base_bucket:
        # **在下载字节之前**先过来源闸 —— 拒绝要发生在出口流量之前, 而且此刻来源
        # 坐标必然在手(contract.assert_source_allowed 的 docstring)。
        assert_source_allowed(
            base_bucket, base_prefix,
            allow_method_validation_only=(rights == "third_party"),
            what=f"r2://{base_bucket}/{base_prefix}")

    qc_gate = load_qc_gate(qc_report, qc_mode)
    session_filter = set(sessions) if sessions else None

    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    if cache is not None:
        cache = Path(cache)
        cache.mkdir(parents=True, exist_ok=True)

    clips = source.iter_clips()
    rejected: Counter = Counter()
    stats: Counter = Counter()
    masked_counts: Counter = Counter()
    verified: Counter = Counter()
    fps_seen: Counter = Counter()
    game_seen: Counter = Counter()
    hash_detail: Counter = Counter()
    resume_changed: Counter = Counter()
    seen_per_session: Counter = Counter()
    chunk_names: dict[str, str] = {}
    written_sessions: set[str] = set()
    rejects: list[dict] = []
    total_frames = 0
    written = skipped_existing = examined = 0
    video_budget = len(clips) if verify < 0 else verify

    def _reject(reason: str, clip: ClipRef, **extra) -> None:
        """拒收必须带**身份**: 聚合计数回答不了"是哪 98 个", 也就交不出去复核。"""
        rejected[reason] += 1
        if len(rejects) < reject_log_cap:
            rejects.append({"reason": reason, "clip": clip.key,
                            "session": clip.session_id, "run": clip.run_uuid,
                            "clip_id": clip.clip_id, **extra})

    def _read_checked(clip: ClipRef, logical: str, *, retry: bool = True) -> bytes | None:
        """读一份对象并核内容寻址哈希; 不过就(可能先清缓存重下一次后)拒收。

        缓存命中的判据只有文件大小 —— 一份大小相同、内容坏掉的缓存文件会让该 clip
        在**此后每一次**跑里都被静默丢掉。所以校验不过先 evict 再重试一次。
        """
        blob = source.read(clip, logical, cache)
        if _sha256_bytes(blob) == clip.files[logical][1]:
            return blob
        if source.evict(clip, logical, cache):
            stats["cache_evicted"] += 1
            if retry:
                return _read_checked(clip, logical, retry=False)
        hash_detail[logical] += 1
        return None

    if qc_gate is not None:
        corpus_sessions = {c.session_id for c in clips.values()}
        if corpus_sessions and not (corpus_sessions & set(qc_gate)):
            raise HaloAdapterError(
                f"QC 报告里 {len(qc_gate)} 个 session 与语料里 {len(corpus_sessions)} "
                "个 session 零交集 —— 十有八九是 session 身份对不上(路径 uuid vs "
                "设备本地 id), 而不是「全部不过闸」。拒绝在这种情况下静默清空语料。")

    def _prefetch_worthy(clip: ClipRef) -> bool:
        """这个 clip 的**正文**值不值得预热(只是猜测, 猜错只影响带宽)。

        车道里约 1/5 不是 Halo, 无差别预热视频等于白花 1/5 的出口流量; 已经产出过
        的 chunk 同理。判定权仍在主循环 —— 这里猜错只会让主循环自己去串行下载。
        """
        if not {"frames.jsonl.zst", "events.jsonl.zst",
                "video.mp4"} <= set(clip.files):
            return False
        try:
            meta = json.loads(source.read(clip, "meta.json", cache))
            if not isinstance(meta, dict):
                return False
            if ((meta.get("target") or {}).get("game_id") != game_id
                    or (meta.get("fps_num"), meta.get("fps_den")) != (fps_num, fps_den)):
                return False
            if not overwrite:
                prev_path = (out / clip.session_id
                             / f'{meta.get("run_id")}-'
                               f'{_segment_id(meta.get("segment_index"))}'
                             / "meta.json")
                if prev_path.exists():
                    prev = json.loads(prev_path.read_text())
                    if (prev.get("provenance") or {}).get("clip_id") == clip.clip_id:
                        return False
        except Exception:  # noqa: BLE001 - 预热的猜测不许中断转换
            return False
        return True

    def _prefetch_candidates(keys: list[str]) -> list[str]:
        """这一批里主循环**真的会去读**的 clip.

        预热必须先跑一遍主循环那几道**不花 I/O** 的选片(session 名单、QC 闸、
        每 session 上限、--limit/--max-examined 余额), 否则:
          * QC floor 会毙掉一半 session, 而预热已经把它们的视频拉下来了 ——
            "不过闸的 session 一个字节都不花"这条承诺就没了;
          * `--max-clips-per-session 2` 的冒烟跑会先拉满整整一批(32 个 clip)。
        计数按批开始时的值算, 只会**少**预热不会多预热(seen_per_session 只增)。
        """
        budget: int | None = None
        if limit is not None:
            budget = max(0, limit - (written + skipped_existing))
        if max_examined is not None:
            left = max(0, max_examined - examined)
            budget = left if budget is None else min(budget, left)
        per_session: Counter = Counter()
        out_keys: list[str] = []
        for k in keys:
            if budget is not None and len(out_keys) >= budget:
                break
            clip = clips[k]
            if session_filter is not None and clip.session_id not in session_filter:
                continue
            if qc_gate is not None and not qc_gate.get(clip.session_id):
                continue
            if (max_clips_per_session is not None
                    and seen_per_session[clip.session_id]
                    + per_session[clip.session_id] >= max_clips_per_session):
                continue
            per_session[clip.session_id] += 1
            out_keys.append(k)
        return out_keys

    def _prefetch(keys: list[str]) -> None:
        if jobs <= 1 or not isinstance(source, R2Source):
            return
        keys = _prefetch_candidates(keys)
        if not keys:
            return
        small = [(clips[k], lg) for k in keys
                 for lg in ("manifest.json", "meta.json") if lg in clips[k].files]
        stats["prefetch_errors"] += _pool_materialize(source, small, cache, jobs)
        body = [(clips[k], lg) for k in keys if _prefetch_worthy(clips[k])
                for lg in ("frames.jsonl.zst", "events.jsonl.zst", "video.mp4")]
        stats["prefetch_errors"] += _pool_materialize(source, body, cache, jobs)

    # 盘: video 的字节数在列举时就知道, 所以"够不够"是可以先算的。这是**上界**
    # (game_id/fps/QC 过滤之后实际会少), 所以只播报不拦; 真正会拦的是跑起来之后
    # 每批复查一次剩余空间 —— 一次 8 小时的跑写满盘, 损失的是那 8 小时。
    need_bytes = sum(sz or 0 for c in clips.values()
                     for name, (_loc, _sha, sz) in c.files.items()
                     if name == "video.mp4")
    free_bytes = None
    if cache is not None:
        import shutil  # noqa: PLC0415 - 只在需要时用
        free_bytes = shutil.disk_usage(cache).free
        if progress:
            print(f"[halo] {len(clips)} clip 待检 · video 上界 "
                  f"{need_bytes / 2**30:.1f} GiB · cache 卷可用 "
                  f"{free_bytes / 2**30:.1f} GiB"
                  + ("  ⚠ 可用空间小于上界(过滤后可能仍然够, 但要盯着)"
                     if free_bytes < need_bytes else ""), file=sys.stderr)

    ordered = sorted(clips)
    stop = False
    t0 = time.monotonic()
    for n_batch, batch in enumerate(_batches(ordered, max(1, prefetch_batch)), 1):
        if stop:
            break
        if cache is not None and min_free_gib > 0:
            import shutil  # noqa: PLC0415
            free = shutil.disk_usage(cache).free
            if free < min_free_gib * 2**30:
                raise HaloAdapterError(
                    f"cache 卷只剩 {free / 2**30:.2f} GiB(<{min_free_gib} GiB), "
                    "停下来而不是把盘写满: 已产出的 chunk 与 cache 都还在, "
                    "腾出空间后原地续跑即可(--min-free-gib 可调)。")
        _prefetch(batch)
        for key in batch:
            if limit is not None and written + skipped_existing >= limit:
                stop = True
                break
            if max_examined is not None and examined >= max_examined:
                stop = True
                break
            clip = clips[key]
            if session_filter is not None and clip.session_id not in session_filter:
                continue
            if qc_gate is not None:
                passed = qc_gate.get(clip.session_id)
                if passed is None:
                    _reject("qc_session_unknown", clip)
                    continue
                if not passed:
                    _reject("qc_session_failed", clip)
                    continue
            if (max_clips_per_session is not None
                    and seen_per_session[clip.session_id] >= max_clips_per_session):
                continue
            # 到这里才算"检查了一个 clip": 上面三道是选片, 不花 I/O
            examined += 1

            if clip.ambiguous:
                # 同一逻辑名在这个前缀下有两代不同内容(重传), 绑哪一代取决于列举
                # 顺序 —— 拒收并报出来, 让人去前缀里去重, 而不是掷骰子
                _reject("ambiguous_clip_files", clip,
                        files=sorted(clip.ambiguous))
                continue
            missing = [name for name in REQUIRED_LOGICAL if name not in clip.files]
            if missing:
                _reject("incomplete_clip", clip, missing=missing)
                continue

            # --- manifest: 文件名内嵌 sha 与 manifest 声明的对账(不用下载正文) ---
            manifest_blob = _read_checked(clip, "manifest.json")
            if manifest_blob is None:
                _reject("hash_mismatch", clip, file="manifest.json")
                continue
            manifest_sha = _sha256_bytes(manifest_blob)
            try:
                manifest = json.loads(manifest_blob)
            except json.JSONDecodeError as e:
                _reject("unreadable_manifest", clip, detail=str(e))
                continue
            if not isinstance(manifest, dict):
                _reject("unreadable_manifest", clip,
                        detail=f"manifest 不是对象而是 {type(manifest).__name__}")
                continue
            name_mismatch = None
            for logical, (_loc, embedded, _size) in clip.files.items():
                if logical == "manifest.json":
                    continue
                declared_sha = _strip_sha_prefix(manifest.get(logical))
                if declared_sha is None or declared_sha != embedded:
                    name_mismatch = logical
                    break
            if name_mismatch:
                # 这不是传输损坏, 是**上游 manifest 与文件名不一致** —— 两者在
                # 报表里长得一样会让人查错方向
                hash_detail["manifest_declares_other_sha"] += 1
                _reject("hash_mismatch", clip, file=name_mismatch,
                        detail="manifest 声明的 sha 与文件名内嵌的不一致")
                continue
            # manifest 声明了、但对象不在: 破 clip。未知载荷(那个孤儿带的 depth.zip)
            # 只计数不拒收 —— 它会在下一步因为缺 target 被正确地拒掉, 拒收理由不该被
            # 一个我们本来就不消费的文件抢走。
            known = set(_LOGICAL_BY_STEM.values())
            absent = [name for name in manifest
                      if name in known and name not in clip.files]
            stats["unknown_payload_files"] += sum(1 for name in manifest
                                                  if name not in known)
            if absent:
                _reject("incomplete_clip", clip, absent=absent)
                continue

            # --- meta: game_id / fps 两道过滤在下载正文之前 ---
            meta_blob = _read_checked(clip, "meta.json")
            if meta_blob is None:
                _reject("hash_mismatch", clip, file="meta.json")
                continue
            verified["meta"] += 1
            try:
                meta = json.loads(meta_blob)
            except json.JSONDecodeError as e:
                _reject("unreadable_meta", clip, detail=str(e))
                continue
            if not isinstance(meta, dict):
                _reject("unreadable_meta", clip,
                        detail=f"meta 不是对象而是 {type(meta).__name__}")
                continue
            # meta 里那第三份哈希: 不核对就等于把一个没人看过的数抄进 provenance,
            # 而那正是复核时拿去跟 R2 对账的字段
            meta_sha_bad = None
            for logical, block in _META_SHA_BLOCKS.items():
                if logical not in clip.files:
                    continue
                declared = _strip_sha_prefix((meta.get(block) or {}).get("sha256"))
                if declared is not None and declared != clip.files[logical][1]:
                    meta_sha_bad = logical
                    break
            if meta_sha_bad:
                hash_detail["meta_declares_other_sha"] += 1
                _reject("hash_mismatch", clip, file=meta_sha_bad,
                        detail="meta 声明的 sha 与文件名/manifest 不一致")
                continue

            target = meta.get("target")
            if not isinstance(target, dict) or not target.get("game_id"):
                # 车道里那个 fallout4 depth 孤儿就是这样混进来的: 没有 target 字段,
                # 只能靠路径猜 —— 所以这里一律拒收, 不猜。
                _reject("missing_target", clip)
                continue
            game_seen[target["game_id"]] += 1
            if target["game_id"] != game_id:
                _reject("game_id_mismatch", clip, game_id=target["game_id"])
                continue

            num, den = meta.get("fps_num"), meta.get("fps_den")
            fps_seen[f"{num}/{den}"] += 1
            if (num, den) != (fps_num, fps_den):
                _reject("fps_mismatch", clip, fps=f"{num}/{den}")
                continue
            probe = meta.get("probe") or {}
            if ("fps_num" in probe and "fps_den" in probe
                    and (probe["fps_num"], probe["fps_den"]) != (num, den)):
                # meta 与容器探针各说各话 = 我们并不知道真实帧率, 而 gate_fps 存在的
                # 意义正是不许这种东西混进一次训练
                _reject("fps_probe_disagreement", clip,
                        meta_fps=f"{num}/{den}",
                        probe_fps=f'{probe["fps_num"]}/{probe["fps_den"]}')
                continue

            segment_id = _segment_id(meta.get("segment_index"))
            chunk_name = f'{meta.get("run_id")}-{segment_id}'
            chunk_key = f"{clip.session_id}/{chunk_name}"
            if chunk_key in chunk_names and chunk_names[chunk_key] != clip.clip_id:
                # 实测 8,744 clip 上 0 命中, 但静默覆盖会让数据凭空消失, 所以给它一个
                # 确定性的后缀而不是让后来者盖掉前一个。sample_key/chunk 都从这个
                # 名字派生(见 _chunk_meta), 否则目录分开了而身份还撞着。
                chunk_name = f"{chunk_name}-{clip.clip_id[:8]}"
                chunk_key = f"{clip.session_id}/{chunk_name}"
                stats["chunk_key_collisions"] += 1
            chunk_names[chunk_key] = clip.clip_id
            chunk_dir = out / clip.session_id / chunk_name

            # --- 断点续跑: 同一批**字节**且同一套**口径**才跳过 ---
            existing = chunk_dir / "meta.json"
            if existing.exists() and not overwrite:
                try:
                    prev = json.loads(existing.read_text())
                except json.JSONDecodeError:
                    prev = {}
                prov = prev.get("provenance") or {}
                same_bytes = (prov.get("clip_id") == clip.clip_id
                              and prov.get("manifest_sha256") == manifest_sha
                              and (chunk_dir / "p0.jsonl").exists())
                # 合规戳/mask/fps/来源都**不是字节的属性**: 只比字节就意味着撤销
                # 一份具结、换个 consent、关掉 mask 之后重跑是一次什么都没发生的
                # 空跑, 而统计报的是新口径。
                changed = [k for k, v in stamp.items()
                           if k != "tags" and prev.get(k) != v]
                if set(stamp["tags"]) - set(prev.get("tags") or ()):
                    changed.append("tags")
                if (prev.get("masked_actions") or {}).get("semantics") != sorted(mask):
                    changed.append("masked_actions")
                if (prev.get("source") or {}).get("bucket") != base_bucket:
                    changed.append("source")
                if prev.get("game") != game_id:
                    changed.append("game")
                prev_video = prev.get("video") or {}
                if (prev_video.get("fps_num"), prev_video.get("fps_den")) \
                        != (fps_num, fps_den):
                    changed.append("fps")
                if prev.get("sample_key") != f"{clip.session_id}_{chunk_name}":
                    changed.append("sample_key")
                if same_bytes and not changed:
                    skipped_existing += 1
                    seen_per_session[clip.session_id] += 1
                    written_sessions.add(clip.session_id)
                    total_frames += int((prev.get("video") or {}).get("frames") or 0)
                    continue
                if same_bytes:
                    # 字节没变但口径变了 → 整块重转(重写 p0.jsonl 与 meta.json,
                    # 不是只补 meta —— mask 变了逐帧标签也得跟着变)
                    stats["chunks_reconverted_settings_changed"] += 1
                    resume_changed.update(sorted(set(changed)))

            # --- 正文 ---
            frames_blob = _read_checked(clip, "frames.jsonl.zst")
            if frames_blob is None:
                _reject("hash_mismatch", clip, file="frames.jsonl.zst")
                continue
            verified["frames"] += 1
            events_blob = _read_checked(clip, "events.jsonl.zst")
            if events_blob is None:
                _reject("hash_mismatch", clip, file="events.jsonl.zst")
                continue
            verified["events"] += 1

            try:
                frames = _jsonl(unzstd(frames_blob))
                events = _jsonl(unzstd(events_blob))
            except UnreadablePayload as e:
                # 一份读不出来的正文只该毙掉这一个 clip, 不是整趟 8,663 clip 的跑
                _reject("unreadable_payload", clip, detail=str(e))
                continue
            if not frames:
                _reject("empty_frames", clip)
                continue
            declared_counts = [meta.get("n_frames"),
                               (meta.get("frames") or {}).get("lines")]
            if probe.get("sample_count") is not None:
                declared_counts.append(probe["sample_count"])
            if any(d is not None and int(d) != len(frames) for d in declared_counts):
                # 帧数三处声明(meta.n_frames / frames.lines / probe.sample_count)必须
                # 一致, 否则"逐帧标签与视频帧 1:1"这条契约就是假的
                _reject("frame_count_mismatch", clip,
                        declared=[d for d in declared_counts if d is not None],
                        actual=len(frames))
                continue
            try:
                frames.sort(key=lambda r: int(_need(r, "frame", "frames.jsonl")))
                if [int(r["frame"]) for r in frames] != list(range(len(frames))):
                    # p0.jsonl 的第 i 行就是视频第 i 帧的标签 —— 缺号或重号会让整段
                    # 标签相对画面平移, 而且平移之后每一项统计看起来都还正常
                    _reject("frame_index_gap", clip)
                    continue
                raw, skipped_abs, orphaned = raw_pointer_by_frame(events, frames)
            except UnreadablePayload as e:
                _reject("unreadable_payload", clip, detail=str(e))
                continue

            empty_events = not events
            if empty_events:
                stats["clips_with_empty_events"] += 1
                if drop_empty_events:
                    _reject("empty_events", clip)
                    continue

            stats["absolute_moves_skipped"] += skipped_abs
            if orphaned:
                # 帧窗口本该首尾相接; 有缝隙意味着落在缝里的位移从每一帧的和里
                # 消失了, 而摘要对账兜不住它(两边同一套窗口)
                stats["pointer_moves_outside_frames"] += orphaned
                stats["clips_with_pointer_gaps"] += 1
            digest, digest_moves = _pointer_digest(frames)
            real_moves = sum(1 for e in events if e.get("type") == MOUSE_MOVE)
            if digest_moves == 0 and real_moves > 0:
                # 该 clip 的逐帧事件摘要没写(不是不一致), 对不了账
                stats["pointer_digest_unavailable_clips"] += 1
            else:
                bad = sum(1 for idx, value in raw.items() if digest.get(idx) != value)
                if bad:
                    stats["pointer_digest_mismatch_frames"] += bad
                    stats["pointer_digest_mismatch_clips"] += 1
                    if strict_pointer_digest:
                        _reject("pointer_digest_mismatch", clip, frames=bad)
                        continue

            got = _video_checked(source, clip, cache, video_budget, stats, hash_detail)
            if got is None:
                _reject("hash_mismatch", clip, file="video.mp4")
                continue
            video_path, hashed, video_budget = got
            if hashed:
                verified["video"] += 1

            chunk_dir.mkdir(parents=True, exist_ok=True)
            link = chunk_dir / "p0.mp4"
            if link.is_symlink() or link.exists():
                link.unlink()
            link.symlink_to(video_path.resolve())
            _write_actions(chunk_dir / "p0.jsonl", frames, raw, mask, stats,
                           masked_counts)

            masked_block = {
                "semantics": sorted(mask),
                "reason": "combat.aim 是 toggle-zoom 语义的坏标签(research/50 §4.2/D5): "
                          "按下切换、再按取消, 「按住瞄准」的帧序列在标签里不成立。"
                          "六通道动作头因此降五通道。",
                "reversible_field": "actions_masked",
            } if mask else {"semantics": [], "reason": None, "reversible_field": None}
            if isinstance(source, R2Source):
                clip_prefix = clip.files["meta.json"][0].rsplit("/", 1)[0]
            else:
                clip_prefix = "/".join(x for x in (
                    base_prefix.rstrip("/"), clip.session_id, "runs", clip.run_uuid,
                    "clips", clip.clip_id) if x)
            meta_out = _chunk_meta(clip=clip, meta=meta, n_frames=len(frames),
                                   chunk_name=chunk_name, stamp=stamp,
                                   masked=masked_block, manifest_sha=manifest_sha,
                                   fps_num=fps_num, fps_den=fps_den,
                                   empty_events=empty_events,
                                   source_bucket=base_bucket,
                                   source_prefix=clip_prefix,
                                   source_observed=isinstance(source, R2Source))
            (chunk_dir / "meta.json").write_text(
                json.dumps(meta_out, indent=1, ensure_ascii=False) + "\n")

            written += 1
            total_frames += len(frames)
            seen_per_session[clip.session_id] += 1
            written_sessions.add(clip.session_id)

        if progress:
            done = min(n_batch * max(1, prefetch_batch), len(ordered))
            el = time.monotonic() - t0
            rate = done / el if el > 0 else 0
            left = max(0.0, (len(ordered) - done) / rate) if rate > 0 else 0.0
            print(f"[halo] {done:>6}/{len(ordered)} clip · "
                  f"产出 {written} · 跳过 {skipped_existing} · 拒收 "
                  f"{sum(rejected.values())} · {total_frames * fps_den / fps_num / 3600:.3f} h · "
                  f"已用 {el / 60:.1f} min · 余 ~{left / 60:.0f} min",
                  file=sys.stderr)

    if rejects_jsonl is not None:
        Path(rejects_jsonl).write_text(
            "".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rejects))

    seconds = total_frames * fps_den / fps_num
    return {
        "source": source.describe(),
        "source_kind": source.kind,
        "source_bucket": base_bucket,
        "source_prefix": base_prefix,
        "source_coordinate": ("observed" if isinstance(source, R2Source)
                              else ("asserted" if base_bucket else "undeclared")),
        "game_id": game_id,
        "fps": f"{fps_num}/{fps_den}",
        "clips_indexed": len(clips),
        "clips_examined": examined,
        "chunks_written": written,
        "chunks_skipped_existing": skipped_existing,
        "chunks_reconverted_settings_changed":
            stats["chunks_reconverted_settings_changed"],
        "resume_settings_changed_fields": dict(sorted(resume_changed.items())),
        "sessions": len(written_sessions),
        "frames": total_frames,
        # doc 44 强制口径: 小时数按 Σ帧数 ÷ fps 计, **禁止**按 clip 数 × 标称时长折算
        "seconds": round(seconds, 3),
        "hours": round(seconds / 3600, 6),
        "rejected": dict(sorted(rejected.items())),
        "rejected_examples": rejects[:20],
        "rejects_logged": len(rejects),
        "hash_mismatch_by_file": dict(sorted(hash_detail.items())),
        "cache_evicted": stats["cache_evicted"],
        "prefetch_errors": stats["prefetch_errors"],
        "clips_with_empty_events": stats["clips_with_empty_events"],
        "frames_beyond_clamp": stats["frames_beyond_clamp"],
        "max_abs_raw_pointer": stats["max_abs_raw_pointer"],
        "masked_actions": sorted(mask),
        "masked_action_frames": stats["masked_action_frames"],
        "masked_action_counts": dict(sorted(masked_counts.items())),
        "absolute_moves_skipped": stats["absolute_moves_skipped"],
        "pointer_moves_outside_frames": stats["pointer_moves_outside_frames"],
        "clips_with_pointer_gaps": stats["clips_with_pointer_gaps"],
        "pointer_digest_mismatch_frames": stats["pointer_digest_mismatch_frames"],
        "pointer_digest_mismatch_clips": stats["pointer_digest_mismatch_clips"],
        "pointer_digest_unavailable_clips": stats["pointer_digest_unavailable_clips"],
        "chunk_key_collisions": stats["chunk_key_collisions"],
        "unknown_payload_files": stats["unknown_payload_files"],
        "sha256_verified": dict(sorted(verified.items())),
        "game_id_seen": dict(sorted(game_seen.items())),
        "fps_seen": dict(sorted(fps_seen.items())),
        "qc_mode": qc_mode,
        "qc_sessions_in_report": len(qc_gate) if qc_gate is not None else None,
        "rights": stamp["rights"],
        "is_trainable": stamp["is_trainable"],
        "tags": stamp["tags"],
    }


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", required=True,
                    help="r2://<bucket>/<prefix> 或本地镜像根目录")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--cache", type=Path, default=None,
                    help="R2 模式必填: 先下载到这里再转换, 支持断点续跑")
    ap.add_argument("--game-id", default=DEFAULT_GAME_ID,
                    help=f"只收 meta.target.game_id 等于它的 clip(默认 "
                         f"{DEFAULT_GAME_ID})。⚠ 授权按作品成立: 具结车道只接受 "
                         f"{sorted(HALO_PARTNER_GAME_IDS)}, 其余作品硬报错")
    ap.add_argument("--fps", default=f"{DEFAULT_FPS_NUM}/{DEFAULT_FPS_DEN}",
                    help="只收该帧率的 clip, 形如 60/1 —— gate_fps 对混合帧率硬报错")
    ap.add_argument("--mask-actions", nargs="*", default=list(DEFAULT_MASK_ACTIONS),
                    help="从每帧 actions 里剔除的语义(默认 combat.aim); "
                         "给空列表可关闭")
    ap.add_argument("--verify", type=int, default=8,
                    help="对多少个「非新下载」的 video 重算 sha256(-1=全部)。"
                         "meta/frames/events 与新下载的 video 恒全验")
    ap.add_argument("--rights", default="third_party", choices=RIGHTS_CHOICES,
                    help="third_party(默认, 不可训练) / first_party(具结车道, "
                         "必须同时给 --consent-id)")
    ap.add_argument("--consent-id", default=None,
                    help="owner 具结 ID, 只在 --rights first_party 下有效")
    ap.add_argument("--qc-report", type=Path, default=None,
                    help="halo_qc 导出的 session 级 QC 结论(JSON)")
    ap.add_argument("--qc-mode", default=None, choices=QC_MODES,
                    help="off / floor(默认, 给了 --qc-report 时) / strict")
    ap.add_argument("--sessions", nargs="+", default=None,
                    help="只转换这些 session uuid")
    ap.add_argument("--max-clips-per-session", type=int, default=None)
    ap.add_argument("--limit", type=int, default=None,
                    help="最多**产出**多少个 chunk(被拒的不算, 所以混车道上它不是"
                         "工作量的上界; 要限制工作量用 --max-examined)")
    ap.add_argument("--max-examined", type=int, default=None,
                    help="最多**检查**多少个 clip(含被拒的) —— 冒烟跑用它才不会"
                         "为了凑够 --limit 把整条车道读一遍")
    ap.add_argument("--drop-empty-events", action="store_true",
                    help="剔除空 events 的 clip。默认**不**剔除 —— 那是真实的操作"
                         "空窗, 语料侧的裁决是降权(chunk 带 empty_events 标签)")
    ap.add_argument("--strict-pointer-digest", action="store_true",
                    help="逐帧事件摘要与半开区间重算不一致时拒收该 clip")
    ap.add_argument("--overwrite", action="store_true",
                    help="不跳过已产出的 chunk。续跑同时判「同一批字节」(clip_id + "
                         "manifest 哈希)与「同一套口径」(合规戳/mask/fps/game/来源), "
                         "口径变了会自动重转; 只有改了输出 meta 的**结构**才需要它")
    ap.add_argument("--jobs", type=int, default=1,
                    help="并发预热到 cache 的线程数(只对 R2 模式有意义)。1=不起"
                         "线程池, 与从前逐字节相同; 全量跑建议 8–24, 见模块 docstring")
    ap.add_argument("--prefetch-batch", type=int, default=32,
                    help="每次并发预热多少个 clip(--jobs>1 时生效)")
    ap.add_argument("--rejects-jsonl", type=Path, default=None,
                    help="把逐条拒收记录(带 clip 身份与理由)写到这个文件 —— "
                         "聚合计数回答不了「是哪 98 个」")
    ap.add_argument("--source-uri", default=None,
                    help="本地镜像模式下申报字节的来源, 形如 "
                         "r2://<bucket>/<prefix> —— 来源闸(contract.SOURCE_RULES)"
                         "与 chunk 的来源戳都读它; R2 模式自动从 --src 取。"
                         "具结车道(--rights first_party)下**必填**")
    ap.add_argument("--credentials-dir", type=Path, default=Path("/tmp"),
                    help="r2-training-{account-id,access-key-id,secret-access-key}.txt "
                         "所在目录(也可用 R2_* 环境变量)")
    ap.add_argument("--stats-json", type=Path, default=None,
                    help="把统计另存一份到这个文件")
    ap.add_argument("--no-progress", action="store_true",
                    help="不打进度行(默认每批往 stderr 打一行: 已检/产出/拒收/"
                         "小时数/ETA —— 一次 8 小时的跑没有进度是不可运维的)")
    ap.add_argument("--min-free-gib", type=float, default=2.0,
                    help="cache 卷剩余空间低于它就停下来(默认 2 GiB)。写满盘损失的"
                         "是已经跑掉的那几个小时, 不是数据")
    a = ap.parse_args()

    try:
        fps_num, _, fps_den = a.fps.partition("/")
        fps_num, fps_den = int(fps_num), int(fps_den or 1)
    except ValueError:
        raise SystemExit(f"--fps 要形如 60/1, 收到 {a.fps!r}")
    qc_mode = a.qc_mode or ("floor" if a.qc_report else "off")

    try:
        stats = convert(
            a.src, a.out, cache=a.cache, game_id=a.game_id,
            fps_num=fps_num, fps_den=fps_den, mask_actions=a.mask_actions,
            verify=a.verify, rights=a.rights, consent_id=a.consent_id,
            qc_report=a.qc_report, qc_mode=qc_mode, sessions=a.sessions,
            max_clips_per_session=a.max_clips_per_session, limit=a.limit,
            max_examined=a.max_examined,
            drop_empty_events=a.drop_empty_events,
            strict_pointer_digest=a.strict_pointer_digest,
            overwrite=a.overwrite, credentials_dir=a.credentials_dir,
            source_uri=a.source_uri, jobs=a.jobs,
            prefetch_batch=a.prefetch_batch, rejects_jsonl=a.rejects_jsonl,
            progress=not a.no_progress, min_free_gib=a.min_free_gib)
    except (HaloAdapterError, ValueError) as e:
        raise SystemExit(str(e))

    text = json.dumps(stats, indent=1, ensure_ascii=False)
    print(text)
    if a.stats_json:
        a.stats_json.write_text(text + "\n")
    if a.rejects_jsonl:
        print(f"→ {a.rejects_jsonl}({stats['rejects_logged']} 条拒收记录)")
    print(f"→ {a.out}")
    if stats["chunks_reconverted_settings_changed"]:
        # 静默重写合规戳是最坏的一种"帮忙"
        print(f"⚠ {stats['chunks_reconverted_settings_changed']} 个已存在的 chunk "
              f"是用**不同口径**产出的, 已按本次参数整块重转; 变化的字段: "
              f"{sorted(stats['resume_settings_changed_fields'])}", file=sys.stderr)
    if stats["cache_evicted"]:
        print(f"⚠ 清掉了 {stats['cache_evicted']} 份校验不过的缓存文件并重下 —— "
              "缓存里出现过坏字节, 值得查一次链路", file=sys.stderr)
    if stats["pointer_moves_outside_frames"]:
        print(f"⚠ {stats['pointer_moves_outside_frames']} 个 mouse_move 落在帧窗口"
              f"的缝隙里({stats['clips_with_pointer_gaps']} 个 clip) —— 「帧窗口首尾"
              "相接」这条前提破了, 重算出来的指针标签是有损的", file=sys.stderr)
    if stats["is_trainable"]:
        print("⚠ 具结车道: 产物 is_trainable=true, 但带 no_publish / "
              "halo_partner_licensed 标签 —— C-1 覆盖采集与内部训练, "
              "对外测试/演示/发布需要 C-2(未取得)。")
    else:
        print("提醒: 产物全部 is_trainable=false(逐字抄源 meta 的 non_trainable=true), "
              "只用于方法验证; 要取数请显式走 --rights first_party --consent-id。")


if __name__ == "__main__":
    main()
