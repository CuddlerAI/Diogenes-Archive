#!/usr/bin/env python3
"""API 摄取 clip 布局 → data-demo-converter RAW 布局：重映射 + 合成生成器（docs/141 §3 S1）。

解决 research/50 §4.1 的「形态闸」：具结车道（attested-trainable）里 API 摄取布局的
session 级对象数为 0——没有 session.json / run.json / catalog snapshot——converter 物理上
无法消费。本工具从 clip meta 的 `catalog` 块 + 仓库 `config/catalogs/` 注册表合成这些
文件（content hash 与 meta 声明逐字节对账），并产出「下载即重命名」的映射计划。

两个子命令：

  plan          纯本地：读 `--session-dump`（`_listing.json` + 预取的 meta-*/manifest-*
                小文件，文件名用 `__` 表示路径层级），产出 remap-plan.json、
                PROVENANCE.json 与全部合成件。任何对不上账的地方都 fail loudly。
  emit-manifest 把 plan 变成 mv_pipeline_driver.mjs 消费的 mv-manifest.json：
                源对象 presigned GET（gets[].rel = RAW 相对路径，下载即重命名）；
                合成件先 PUT 到 staging 前缀再以 presigned GET 加进 gets。
                ⚠ 本子命令的网络路径（boto3 上传/presign）**未联网实测**；
                presign 用 SigV4（R2 必须，见本目录 README），凭据只从环境变量读，
                永不打印。

红线（docs/141 §0-2）：绝不合成 `consented_*` 状态或 `non_trainable=false`——那是
93/44 号明令的「回填授权状态」造假。本工具在读入侧断言车道如实（全部 meta 必须
non_trainable=true、无 consent_id），在写出侧再用 `_assert_red_lines` 扫一遍产物。

对账原则（docs/141 §3 S1）：核验 = 对拍 manifest.json 登记的 sha 与文件名内嵌 sha，
不重哈希大对象；本地已有字节的小文件（meta/manifest/catalog snapshot）逐字节重哈希。

「validator 强制字段表」提炼自 DataMiner/tools/data-demo-converter/src/lib.rs
（validate_session_header ~L2059 / validate_ready_session ~L1892 / validate_run ~L3641 /
validate_segment ~L2160 / validate_v2_catalog_snapshots ~L3722），见 VALIDATOR_RULES。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------
# lib.rs 提炼出的常量（逐条对应 converter 侧定义，勿凭记忆改）
# ---------------------------------------------------------------------------

# capture-core/src/audio_contract.rs L6-L17
RECORDING_SCHEMA_V1 = "dm-local-dev-recording-1"
RECORDING_SCHEMA_V2 = "dm-local-dev-recording-2"
SESSION_SCHEMA_V2 = "dm-local-dev-session-2"
SESSION_SCHEMA_V3 = "dm-local-dev-session-3"
AUDIO_ARTIFACT_NAME = "audio.pcm.zst"
AUDIO_SAMPLE_RATE_HZ = 48_000

# lib.rs L50-L59
RAW_READY_STATE = "non_trainable_ready"
RAW_PLAIN_READY_STATE = "ready"  # attested 车道接受（lib.rs L2219-L2226）
RAW_RUN_COMPLETE_STATE = "non_trainable_complete"
CATALOG_MODE_DEV = "dev_non_trainable"

# lib.rs ensure_contract L3899-L3909
CONTRACT_DM1 = {
    "version": "dm-1",
    "pointer_clamp": 200,
    "pointer_mu": 8,
    "pointer_ref": 200,
    "pointer_max_bin": 10,
}

# capture-core/src/profile.rs L13-L14 + source_geometry_rule/allows_source_scaling
NON_TRAINABLE_MIN_SOURCE_WIDTH = 960
NON_TRAINABLE_MIN_SOURCE_HEIGHT = 540

# 段目录内的四/五件套（audio_contract.rs BASE_PAYLOAD_NAMES + AUDIO_ARTIFACT_NAME）
BASE_PAYLOAD_NAMES = ("video.mp4", "frames.jsonl.zst", "events.jsonl.zst", "meta.json")

# API clip 对象名 → RAW 段内文件名
KIND_TO_RAW_NAME = {
    "video": "video.mp4",
    "frames": "frames.jsonl.zst",
    "events": "events.jsonl.zst",
    "audio": AUDIO_ARTIFACT_NAME,
    "meta": "meta.json",
}
KIND_EXPECTED_EXT = {
    "video": "mp4",
    "frames": "jsonl.zst",
    "events": "jsonl.zst",
    "audio": "pcm.zst",
    "meta": "json",
    "manifest": "json",
}

SNAPSHOT_FILES = (
    # (RAW 文件名, meta catalog 块里声明 content hash 的取值函数)
    ("catalog.json", lambda cat: cat["catalog_content_hash"]),
    ("device-profile.json", lambda cat: cat["device"]["content_hash"]),
    ("recording-profile.json", lambda cat: cat["recording"]["content_hash"]),
    ("game-profile.json", lambda cat: cat["game"]["content_hash"]),
    ("mapping.json", lambda cat: cat["mapping"]["content_hash"]),
)

_KEY_RE = re.compile(r"(?:^|/)sessions/(?P<session>[0-9a-fA-F-]{36})/(?P<rest>.+)$")
_CLIP_RE = re.compile(
    r"^runs/(?P<run>[0-9a-fA-F-]{36})/clips/(?P<clip>[0-9a-fA-F-]{36})/"
    r"(?P<kind>[a-z0-9_]+)-(?P<sha>[0-9a-f]{64})\.(?P<ext>.+)$"
)
_SAFE_COMPONENT_RE = re.compile(r"^[A-Za-z0-9_-]+$")  # lib.rs ensure_safe_component L3911


class PlanError(RuntimeError):
    """任何对不上账、缺件、断裂、红线触发——统一 fail loudly。"""


def _sha256_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def _fail(msg: str) -> None:
    raise PlanError(msg)


def _ensure(cond: bool, msg: str) -> None:
    if not cond:
        _fail(msg)


# ---------------------------------------------------------------------------
# dump 读入
# ---------------------------------------------------------------------------


@dataclass
class ClipObjects:
    """一个 clip 目录下的对象（按 kind 分）：kind -> (key, sha, ext, size)。"""

    api_run: str
    clip_id: str
    objects: dict = field(default_factory=dict)
    meta: dict | None = None
    manifest: dict | None = None


@dataclass
class SessionDump:
    api_session: str
    prefix: str  # 到 sessions/<uuid>/ 为止（含尾斜杠）的 key 前缀
    clips: dict = field(default_factory=dict)  # (api_run, clip_id) -> ClipObjects
    sidecars: list = field(default_factory=list)  # [{key, rest, size}]


def load_dump(dump_dir: Path) -> dict[str, SessionDump]:
    """读 _listing.json 与预取的 meta/manifest 小文件，按 API session 分组。"""
    listing_path = dump_dir / "_listing.json"
    _ensure(listing_path.is_file(), f"session dump 缺 _listing.json: {listing_path}")
    listing = json.loads(listing_path.read_text(encoding="utf-8"))
    _ensure(isinstance(listing, list) and listing, "_listing.json 必须是非空对象数组")

    sessions: dict[str, SessionDump] = {}
    for obj in listing:
        key, size = obj["key"], obj["size"]
        m = _KEY_RE.search(key)
        _ensure(m is not None, f"无法从 key 解析 sessions/<uuid>/: {key}")
        sid = m.group("session")
        rest = m.group("rest")
        dump = sessions.setdefault(
            sid, SessionDump(api_session=sid, prefix=key[: m.end("session") + 1])
        )
        cm = _CLIP_RE.match(rest)
        if cm is None:
            # runs/ 层（如 camera-pose-index.jsonl，docs/133）或 session 层散件：
            # 归类 sidecar，不进 RAW 段。
            dump.sidecars.append({"key": key, "rest": rest, "size": size})
            continue
        kind = cm.group("kind")
        _ensure(
            kind in KIND_EXPECTED_EXT,
            f"clip 对象出现未知 kind={kind}（不敢静默丢弃）: {key}",
        )
        _ensure(
            cm.group("ext") == KIND_EXPECTED_EXT[kind],
            f"clip 对象扩展名与 kind 不符: {key}",
        )
        clip = dump.clips.setdefault(
            (cm.group("run"), cm.group("clip")),
            ClipObjects(api_run=cm.group("run"), clip_id=cm.group("clip")),
        )
        _ensure(
            kind not in clip.objects,
            f"clip {cm.group('clip')} 出现重复 kind={kind}",
        )
        clip.objects[kind] = {
            "key": key,
            "sha": cm.group("sha"),
            "ext": cm.group("ext"),
            "size": size,
        }

    # 装载预取的 meta / manifest 字节并逐字节核验（本地有字节的小文件才重哈希）
    for dump in sessions.values():
        for (api_run, clip_id), clip in dump.clips.items():
            for kind in ("meta", "manifest"):
                _ensure(
                    kind in clip.objects,
                    f"clip {clip_id} 缺 {kind} 对象（listing 里没有）",
                )
                entry = clip.objects[kind]
                local_name = entry["key"][len(dump.prefix):].replace("/", "__")
                local = dump_dir / local_name
                _ensure(local.is_file(), f"dump 缺预取文件 {local_name}")
                data = local.read_bytes()
                actual = hashlib.sha256(data).hexdigest()
                _ensure(
                    actual == entry["sha"],
                    f"{local_name} 本地字节 sha256 ({actual}) 与文件名内嵌 sha 不符",
                )
                parsed = json.loads(data)
                if kind == "meta":
                    clip.meta = parsed
                else:
                    clip.manifest = parsed
    return sessions


# ---------------------------------------------------------------------------
# 一致性 + validator 字段表 预检
# ---------------------------------------------------------------------------

# 提炼自 lib.rs 的强制字段表（合成与预检都以此为准；validator 不读的字段不合成臆造值）
VALIDATOR_RULES = {
    "session.json": [
        "schema_version ∈ {dm-local-dev-session-1/2/3}；有音频段 ⇒ 必须 -3（L2069/L2121/L2292）",
        "non_trainable == true（local-debug/attested，L2082-L2097）；production_certified == false（L2099）",
        "attested 档下不得携带 consent_id（L2090-L2096）",
        "session_id 是安全路径组件（字母数字-_，L2098）",
        "contract == dm-1 常量组（L2103, ensure_contract L3899）",
        "capture_policy: source==game_window_only；width/height/fps/gop == recording profile；"
        "scale/minimum_source_* 按 non_trainable 几何规则（960×540 下限）；"
        "audio == recording profile 的 audio 块（无则 false）（validate_capture_policy L3951）",
        "mapping{id,version,content_hash} == mapping.json 快照；recording_profile_id == mapping 声明（L2130/L2144）",
        "game.id == mapping.json 的 game.id（L2140）",
        "catalog 块（v2/v3）: mode==dev_non_trainable；provenance ∈ {compiled,delivered}；"
        "device/recording/game/mapping 四组 id/version/content_hash 齐全（validate_v2_catalog_snapshots L3722）",
        "catalog 块必须与每个 segment meta 的 catalog 块完全相等（L2262）",
    ],
    "catalog snapshots（5 份，逐字节 sha 对账 L3878）": [
        "catalog.json: schema dm-catalog-bundle-2、catalog_id/version 与声明一致、non_trainable==true、"
        "device/recording/game/mapping 各恰有一条 id+version 匹配且 content_hash 一致（L3776-L3792）",
        "device-profile.json: schema dm-device-profile-2、profile_id/version、non_trainable==true（L3794）",
        "game-profile.json: schema dm-game-profile-2、game_id/version、non_trainable、game_id==session.game.id、"
        "device_profile_id/recording_profile_id/mapping_id 交叉引用（L3810）",
        "recording-profile.json: schema dm-recording-profile-1、profile_id/version、non_trainable、game_id 交叉（L3823）",
        "mapping.json: sha==声明、mapping_id/version、game.id、recording_profile_id 交叉（L3833）",
    ],
    "run.json": [
        "schemaVersion == 段 schema（音频代 dm-local-dev-recording-2，L3647）",
        "state == non_trainable_complete；nonTrainable==true；productionCertified==false（L3656-L3669）",
        "sessionId == session.json；runId=r+数字 且 == 目录名 且按枚举序 r0000..（L3670/L1934）",
        "mappingId/mappingVersion/mappingContentHash == mapping.json 快照（L3678-L3689）",
        "sessionFrameStart 跨 run 连续、首 run 从 0 起；sessionFrameEnd == start+frameCount（L1940/L2018）",
        "frameCount == Σ段帧数；segmentCount == 段目录数；inputEventCount == Σ段 events.lines（L1948/L1988/L2013）",
        "inputQueueOverflows == 0（L2017）",
        "qpcFrequency 可选；若写必须与音频段 qpc_frequency 一致、跨 run 一致（L1925/L2297）",
        "anchorQpc/anchorAtUtc 可选（缺省只损失墙钟估计，L560-L570）",
    ],
    "segment（meta 侧可预检的部分）": [
        "schema == 会话代对应段 schema；session_id/run_id/segment_index 与目录一致（L2186-L2205）",
        "state ∈ {non_trainable_ready, ready}（attested，L2219）；non_trainable==true；无 consent_id；"
        "production_certified==false（L2227-L2239）",
        "input_status == raw_input_v1；qpc_end > qpc_start（L2241-L2248）",
        "fps/width/height == recording profile（L2249-L2256）；mapping 三元组一致（L2257）",
        "catalog 块 == session.json 的 catalog 块（L2262）；contract dm-1（L2267）",
        "health.queue_drops==0 且 health.input_overflow==0（L2268-L2275）",
        "encoder 身份非空（L4020）；probe: H.264 High、尺寸/fps 匹配、sample_count==n_frames、"
        "clean point、PTS 单调（L3994）",
        "video/frames/events path 固定名（L2278-L2289）；音频代必须带 audio 块（L2291）",
        "sha256.json 键集 == 四件套(+audio.pcm.zst 若有件)；各 sha == meta 登记（L2320-L2359）",
        "video 字节数 == meta.video.bytes（L2345）；audio 压缩字节数 == audio.bytes（L2469）",
        "frames.lines == n_frames（L2375-L2381）",
        "音频 ready/degraded: first_sample_qpc==qpc_start、frames==round(区间×48k/freq)（L2303-L2317）",
    ],
}


def _audio_has_artifact(audio: dict | None) -> bool:
    return audio is not None and audio.get("state") in ("ready", "degraded")


def _expected_manifest_keys(audio: dict | None) -> set:
    names = set(BASE_PAYLOAD_NAMES)
    if _audio_has_artifact(audio):
        names.add(AUDIO_ARTIFACT_NAME)
    return names


def _check_red_line_meta(clip: ClipObjects) -> None:
    meta = clip.meta
    state = meta.get("state")
    _ensure(
        meta.get("non_trainable") is True
        and not meta.get("production_certified", False)
        and meta.get("consent_id") in (None, "")
        and isinstance(state, str)
        and not state.startswith("consented"),
        f"clip {clip.clip_id} 的 meta 治理位越界（non_trainable={meta.get('non_trainable')}, "
        f"state={state}, consent_id={meta.get('consent_id')}）——具结车道必须如实 non-trainable，"
        "拒绝处理疑似车道污染的对象（docs/141 §0-2 红线）",
    )
    _ensure(
        state in (RAW_READY_STATE, RAW_PLAIN_READY_STATE),
        f"clip {clip.clip_id} 段状态 {state} 不是 attested 车道可接受的 ready 态",
    )


def _check_clip(clip: ClipObjects) -> None:
    """单 clip：manifest 登记 sha ↔ 文件名内嵌 sha ↔ meta 登记 sha 三方对拍。"""
    meta, manifest = clip.meta, clip.manifest
    cid = clip.clip_id
    audio_meta = meta.get("audio")
    expected = _expected_manifest_keys(audio_meta)
    _ensure(
        set(manifest.keys()) == expected,
        f"clip {cid} manifest 键集 {sorted(manifest)} ≠ 期望 {sorted(expected)}",
    )
    # 必备对象齐全
    need_kinds = {"video", "frames", "events", "meta", "manifest"}
    if _audio_has_artifact(audio_meta):
        need_kinds.add("audio")
    missing = need_kinds - set(clip.objects)
    _ensure(not missing, f"clip {cid} listing 缺对象: {sorted(missing)}")
    extra = set(clip.objects) - need_kinds
    _ensure(not extra, f"clip {cid} 出现多余对象 kind: {sorted(extra)}")

    # manifest 登记 ↔ 文件名内嵌 sha（不重哈希大对象，docs/141 §3 S1）
    for kind, raw_name in KIND_TO_RAW_NAME.items():
        if kind not in clip.objects:
            continue
        declared = manifest[raw_name]
        embedded = "sha256:" + clip.objects[kind]["sha"]
        _ensure(
            declared == embedded,
            f"clip {cid} {raw_name}: manifest 登记 {declared} ≠ 文件名内嵌 {embedded}",
        )
    # meta 登记 ↔ manifest 登记
    for raw_name, meta_block in (
        ("video.mp4", meta["video"]),
        ("frames.jsonl.zst", meta["frames"]),
        ("events.jsonl.zst", meta["events"]),
    ):
        _ensure(
            meta_block["sha256"] == manifest[raw_name],
            f"clip {cid} {raw_name}: meta 登记 sha ≠ manifest 登记 sha",
        )
        _ensure(
            meta_block["path"] == raw_name,
            f"clip {cid} meta 里 {raw_name} 的 path 非固定名: {meta_block['path']}",
        )
    _ensure(
        meta["video"]["bytes"] == clip.objects["video"]["size"],
        f"clip {cid} video 字节数 meta({meta['video']['bytes']}) ≠ listing({clip.objects['video']['size']})",
    )
    if _audio_has_artifact(audio_meta):
        _ensure(
            audio_meta.get("sha256") == manifest[AUDIO_ARTIFACT_NAME],
            f"clip {cid} audio sha meta ≠ manifest",
        )
        _ensure(
            audio_meta.get("bytes") == clip.objects["audio"]["size"],
            f"clip {cid} audio 字节数 meta({audio_meta.get('bytes')}) ≠ listing({clip.objects['audio']['size']})",
        )

    # validator 段级可预检项（在 mac 上先把 Windows converter 必拒的账拍出来）
    _ensure(meta.get("input_status") == "raw_input_v1", f"clip {cid} input_status 越界")
    _ensure(meta["qpc_end"] > meta["qpc_start"], f"clip {cid} QPC 区间为空")
    health = meta.get("health", {})
    _ensure(
        health.get("queue_drops", 0) == 0 and health.get("input_overflow", 0) == 0,
        f"clip {cid} health 有 queue_drops/input_overflow，converter 必拒",
    )
    enc = meta.get("effective_encoder", {})
    _ensure(
        any(
            (enc.get(k) or "").strip()
            for k in ("transform_clsid", "friendly_name", "hardware_url")
        ),
        f"clip {cid} 编码器身份缺失",
    )
    probe = meta.get("probe", {})
    _ensure(
        probe.get("codec") == "H.264"
        and probe.get("h264_profile") == 100
        and probe.get("width") == meta["width"]
        and probe.get("height") == meta["height"]
        and probe.get("fps_num") == meta["fps_num"]
        and probe.get("fps_den") == meta["fps_den"]
        and probe.get("sample_count") == meta["n_frames"]
        and probe.get("first_sample_clean_point") is True
        and probe.get("pts_monotonic") is True,
        f"clip {cid} probe 与 meta 不自洽（converter validate_probe 必拒）",
    )
    _ensure(
        meta["frames"]["lines"] == meta["n_frames"],
        f"clip {cid} frames.lines ≠ n_frames",
    )
    _ensure(
        _contract_matches(meta.get("contract", {})),
        f"clip {cid} contract 非 dm-1 常量组",
    )
    if _audio_has_artifact(audio_meta):
        freq = audio_meta["qpc_frequency"]
        _ensure(
            audio_meta.get("first_sample_qpc") == meta["qpc_start"],
            f"clip {cid} 音频未对齐视频 QPC 起点",
        )
        ticks = meta["qpc_end"] - meta["qpc_start"]
        want = (ticks * AUDIO_SAMPLE_RATE_HZ + freq // 2) // freq
        _ensure(
            audio_meta.get("frames") == want,
            f"clip {cid} 音频帧数 {audio_meta.get('frames')} ≠ QPC 区间闭合值 {want}",
        )


def _contract_matches(contract: dict) -> bool:
    try:
        return (
            contract["version"] == CONTRACT_DM1["version"]
            and contract["pointer_clamp"] == 200
            and float(contract["pointer_mu"]) == 8.0
            and contract["pointer_ref"] == 200
            and contract["pointer_max_bin"] == 10
        )
    except (KeyError, TypeError, ValueError):
        return False


# ---------------------------------------------------------------------------
# catalog snapshot 解析（内容寻址：工作区扫描 → git 历史回捞；永不合成）
# ---------------------------------------------------------------------------


def _index_catalogs_dir(catalogs_dir: Path) -> dict[str, Path]:
    index: dict[str, Path] = {}
    for path in sorted(catalogs_dir.rglob("*.json")):
        if path.is_file():
            index.setdefault(_sha256_bytes(path.read_bytes()), path)
    return index


def _git_recover(catalogs_dir: Path, want_hash: str, file_name: str) -> tuple[bytes, str] | None:
    """在 git 历史里按内容找回被 bump 掉的旧版 snapshot（逐字节 sha 匹配才算数）。"""
    try:
        repo = subprocess.run(
            ["git", "-C", str(catalogs_dir), "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    repo_path = Path(repo).resolve()
    candidates = [
        p.resolve().relative_to(repo_path).as_posix()
        for p in sorted(catalogs_dir.rglob(file_name))
    ]
    for rel in candidates:
        try:
            revs = subprocess.run(
                ["git", "-C", repo, "log", "--format=%H", "--", rel],
                capture_output=True, text=True, check=True,
            ).stdout.split()
        except subprocess.CalledProcessError:
            continue
        for rev in revs[:500]:
            shown = subprocess.run(
                ["git", "-C", repo, "show", f"{rev}:{rel}"],
                capture_output=True, check=False,
            )
            if shown.returncode != 0:
                continue
            if _sha256_bytes(shown.stdout) == want_hash:
                return shown.stdout, f"git:{rev[:12]}:{rel}"
    return None


def resolve_snapshots(
    catalogs_dir: Path, catalog_block: dict, git_fallback: bool = True
) -> dict[str, dict]:
    """按 meta `catalog` 块声明的 content hash 找齐 5 份 snapshot 字节。

    对不上是硬错误（fail loudly）——绝不合成无法核验的 snapshot。
    """
    index = _index_catalogs_dir(catalogs_dir)
    resolved: dict[str, dict] = {}
    missing: list[str] = []
    for name, hash_of in SNAPSHOT_FILES:
        want = hash_of(catalog_block)
        _ensure(
            isinstance(want, str) and want.startswith("sha256:") and len(want) == 71,
            f"catalog 块对 {name} 声明的 content hash 形状非法: {want!r}",
        )
        if want in index:
            path = index[want]
            resolved[name] = {
                "bytes": path.read_bytes(),
                "sha256": want,
                "resolved_from": str(path),
            }
            continue
        if git_fallback:
            hit = _git_recover(catalogs_dir, want, name)
            if hit is not None:
                data, provenance = hit
                resolved[name] = {
                    "bytes": data,
                    "sha256": want,
                    "resolved_from": provenance,
                }
                continue
        missing.append(f"{name} 声明 {want}")
    if missing:
        _fail(
            "catalog snapshot 与 meta 声明对不上账（工作区与 git 历史均无逐字节匹配），"
            "绝不合成：\n  " + "\n  ".join(missing)
            + f"\n  （查找范围: {catalogs_dir}）"
        )
    _cross_check_snapshots(resolved, catalog_block)
    return resolved


def _cross_check_snapshots(resolved: dict[str, dict], cat: dict) -> None:
    """镜像 validate_v2_catalog_snapshots（L3722-L3850）的身份/交叉引用检查。"""
    catalog = json.loads(resolved["catalog.json"]["bytes"])
    _ensure(
        catalog.get("schema_version") == "dm-catalog-bundle-2"
        and catalog.get("catalog_id") == cat["catalog_id"]
        and catalog.get("catalog_version") == cat["catalog_version"]
        and catalog.get("non_trainable") is True,
        "catalog.json 身份/分类与 meta catalog 块不符",
    )
    for list_name, entry_key in (
        ("device_profiles", "device"),
        ("recording_profiles", "recording"),
        ("game_profiles", "game"),
        ("mappings", "mapping"),
    ):
        want = cat[entry_key]
        matches = [
            e
            for e in catalog.get(list_name, [])
            if e.get("id") == want["id"] and e.get("version") == want["version"]
        ]
        _ensure(
            len(matches) == 1 and matches[0].get("content_hash") == want["content_hash"],
            f"catalog.json {entry_key} 选择与 meta 声明不符",
        )
    device = json.loads(resolved["device-profile.json"]["bytes"])
    _ensure(
        device.get("schema_version") == "dm-device-profile-2"
        and device.get("profile_id") == cat["device"]["id"]
        and device.get("version") == cat["device"]["version"]
        and device.get("non_trainable") is True,
        "device-profile.json 身份与 meta 声明不符",
    )
    game = json.loads(resolved["game-profile.json"]["bytes"])
    _ensure(
        game.get("schema_version") == "dm-game-profile-2"
        and game.get("game_id") == cat["game"]["id"]
        and game.get("version") == cat["game"]["version"]
        and game.get("non_trainable") is True
        and game.get("device_profile_id") == cat["device"]["id"]
        and game.get("recording_profile_id") == cat["recording"]["id"]
        and game.get("mapping_id") == cat["mapping"]["id"],
        "game-profile.json 身份/交叉引用与 meta 声明不符",
    )
    recording = json.loads(resolved["recording-profile.json"]["bytes"])
    _ensure(
        recording.get("schema_version") == "dm-recording-profile-1"
        and recording.get("profile_id") == cat["recording"]["id"]
        and recording.get("version") == cat["recording"]["version"]
        and recording.get("non_trainable") is True
        and recording.get("game_id") == cat["game"]["id"],
        "recording-profile.json 身份/交叉引用与 meta 声明不符",
    )
    mapping = json.loads(resolved["mapping.json"]["bytes"])
    mapping_body = mapping.get("mapping", mapping)
    _ensure(
        mapping_body.get("mapping_id") == cat["mapping"]["id"]
        and mapping_body.get("version") == cat["mapping"]["version"]
        and mapping_body.get("game", {}).get("id") == cat["game"]["id"]
        and mapping_body.get("recording_profile_id") == cat["recording"]["id"],
        "mapping.json 身份/交叉引用与 meta 声明不符",
    )


# ---------------------------------------------------------------------------
# 合成
# ---------------------------------------------------------------------------


def _dump_json(obj) -> bytes:
    return (json.dumps(obj, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def synthesize_session_json(
    meta0: dict, recording_profile: dict, mapping_snapshot: dict
) -> dict:
    """镜像客户端写侧 capture-core/src/recording.rs L3117-L3163 的形状。

    每个字段的证据来源见 PROVENANCE；治理位如实拷贝（读入侧已断言 non_trainable=true）。
    """
    audio_enabled = meta0.get("audio") is not None
    mapping_body = mapping_snapshot.get("mapping", mapping_snapshot)
    cat = meta0["catalog"]
    non_trainable = recording_profile["non_trainable"]
    _ensure(non_trainable is True, "recording profile 声称可训练，具结车道拒绝")
    session = {
        "schema_version": SESSION_SCHEMA_V3 if audio_enabled else SESSION_SCHEMA_V2,
        "session_id": meta0["session_id"],
        "non_trainable": True,  # 红线：只可能是 true（读入侧已断言）
        "production_certified": False,
        "game": {
            "id": meta0["target"]["game_id"],
            "exe_hash": meta0["target"]["executable_sha256"],
        },
        "mapping": {
            "id": mapping_body["mapping_id"],
            "version": mapping_body["version"],
            "content_hash": cat["mapping"]["content_hash"],
            "recording_profile_id": mapping_body["recording_profile_id"],
        },
        "catalog": cat,  # 与每个 segment meta 完全相等（L2262 强制）
        "capture_policy": {
            "source": "game_window_only",
            "width": recording_profile["width"],
            "height": recording_profile["height"],
            # non_trainable 车道几何规则：AtLeast(960,540) + 允许缩放（profile.rs L252/L271）
            "scale": bool(non_trainable),
            "minimum_source_width": NON_TRAINABLE_MIN_SOURCE_WIDTH,
            "minimum_source_height": NON_TRAINABLE_MIN_SOURCE_HEIGHT,
            "fps_num": recording_profile["fps_num"],
            "fps_den": recording_profile["fps_den"],
            "avg_bitrate_bps": recording_profile["bitrate_bps"],
            "max_bitrate_bps": recording_profile["max_bitrate_bps"],
            "segment_seconds": recording_profile["segment_seconds"],
            "gop_frames": recording_profile["gop_frames"],
            "audio": recording_profile.get("audio") if audio_enabled else False,
        },
        "contract": dict(CONTRACT_DM1),
        # 写侧常量（recording.rs L3162），非会话事实；在 PROVENANCE 里标 writer_constant
        "todo": ["annotator_device_consent_grant", "crash_recovery"],
    }
    if audio_enabled:
        _ensure(
            recording_profile.get("audio") is not None,
            "段带音频但 recording profile 无 audio 块——capture_policy.audio 无法如实合成",
        )
    return session


def synthesize_run_json(run_id: str, segs: list[dict]) -> tuple[dict, list[dict]]:
    """从该 run 的全部 segment meta 聚合 run.json；返回 (run_json, unknown_fields)。

    validator 读的字段全部可从 meta 精确聚合（见 VALIDATOR_RULES['run.json']）；
    validator 不读且聚合不出的字段一律**省略**而不是臆造，登记进 synthesized_unknown。
    """
    segs = sorted(segs, key=lambda m: m["segment_index"])
    first, last = segs[0], segs[-1]
    frame_count = sum(m["n_frames"] for m in segs)
    audio_freqs = {
        m["audio"]["qpc_frequency"] for m in segs if m.get("audio") is not None
    }
    _ensure(len(audio_freqs) <= 1, f"run {run_id} 各段音频 qpc_frequency 不一致")
    run = {
        "schemaVersion": segs[0]["schema_version"],
        "state": RAW_RUN_COMPLETE_STATE,
        "nonTrainable": True,
        "productionCertified": False,
        "inputStatus": segs[0]["input_status"],
        "sessionId": segs[0]["session_id"],
        "runId": run_id,
        "mappingId": segs[0]["mapping"]["id"],
        "mappingVersion": segs[0]["mapping"]["version"],
        "mappingContentHash": segs[0]["mapping"]["content_hash"],
        "sessionFrameStart": first["session_frame_start"],
        "sessionFrameEnd": first["session_frame_start"] + frame_count,
        "frameCount": frame_count,
        "segmentCount": len(segs),
        "duplicateFrames": sum(m["health"].get("duplicate_frames", 0) for m in segs),
        "sourceSuperseded": sum(m["health"].get("source_superseded", 0) for m in segs),
        "inputEventCount": sum(m["events"]["lines"] for m in segs),
        "inputQueueOverflows": sum(m["health"].get("input_overflow", 0) for m in segs),
        "qpcStart": first["qpc_start"],
        "qpcEnd": last["qpc_end"],
    }
    if audio_freqs:
        run["qpcFrequency"] = next(iter(audio_freqs))
    unknown = [
        {
            "file": f"runs/{run_id}/run.json",
            "field": "anchorQpc/anchorAtUtc",
            "treatment": "omitted",
            "why": "录制端墙钟配对读数不可复原；converter 侧为可选字段（RawRun serde default），"
                   "缺省时墙钟估计回退到 session_id 内嵌 epoch（lib.rs WallClockAnchor::new）",
        },
        {
            "file": f"runs/{run_id}/run.json",
            "field": "stopReason",
            "treatment": "omitted",
            "why": "停录原因不可复原；converter 不读该字段",
        },
        {
            "file": f"runs/{run_id}/run.json",
            "field": "inputQueueOverflows",
            "treatment": "derived_from_segments",
            "why": "run 级计数器不可复原，取 Σ段 health.input_overflow（本批全 0）；"
                   "理论上段间溢出可未落段计数——converter 强制该值为 0，若真发生过，"
                   "Windows 侧 convert 会在别处（事件重投影）暴露",
        },
        {
            "file": f"runs/{run_id}/run.json",
            "field": "qpcStart/qpcEnd/duplicateFrames/sourceSuperseded",
            "treatment": "derived_from_segments",
            "why": "converter 不读；由段 meta 精确聚合，仅供人读",
        },
    ]
    if not audio_freqs:
        unknown.append(
            {
                "file": f"runs/{run_id}/run.json",
                "field": "qpcFrequency",
                "treatment": "omitted",
                "why": "无音频段时该值无本地证据；converter 侧可选，缺省只损失墙钟估计",
            }
        )
    return run, unknown


FORBIDDEN_PATTERNS = ("consented", '"non_trainable": false', '"nonTrainable": false', '"consent_id"')


def _assert_red_lines(rel: str, data: bytes) -> None:
    text = data.decode("utf-8")
    for pat in FORBIDDEN_PATTERNS:
        _ensure(
            pat not in text,
            f"红线自检失败：合成件 {rel} 含 {pat!r}（docs/141 §0-2）",
        )


# ---------------------------------------------------------------------------
# plan 主流程
# ---------------------------------------------------------------------------


def build_plan(
    dumps: dict[str, SessionDump],
    catalogs_dir: Path,
    game_allowlist: set[str],
    git_fallback: bool = True,
) -> dict:
    """纯内存 plan：返回 {sessions:[...], rejected:{...}, provenance:{...}}。"""
    plan_sessions = []
    rejected_sessions = []
    provenance_sessions = []

    for api_session, dump in sorted(dumps.items()):
        _ensure(dump.clips, f"API session {api_session} 没有任何 clip")
        # ---- game 闸（session 级：任何一个 clip 越界即整个 session 淘汰）----
        games = sorted(
            {c.meta["target"]["game_id"] for c in dump.clips.values()}
            | {c.meta["catalog"]["game"]["id"] for c in dump.clips.values()}
        )
        outside = [g for g in games if g not in game_allowlist]
        if outside:
            rejected_sessions.append(
                {
                    "api_session": api_session,
                    "reason": "game_allowlist",
                    "games_seen": games,
                    "games_rejected": outside,
                    "clip_count": len(dump.clips),
                }
            )
            continue

        # ---- 红线 + 单 clip 对账 ----
        for clip in dump.clips.values():
            _check_red_line_meta(clip)
            _check_clip(clip)

        # ---- session 级一致性 ----
        metas = [c.meta for c in dump.clips.values()]
        m0 = metas[0]
        for key in ("session_id", "schema_version"):
            vals = {json.dumps(m.get(key), sort_keys=True) for m in metas}
            _ensure(len(vals) == 1, f"session {api_session} 各 clip 的 {key} 不一致")
        cat0 = m0["catalog"]
        for m in metas:
            _ensure(
                m["catalog"] == cat0,
                f"session {api_session} 各 clip 的 catalog 块不一致——"
                "无法合成单一 session.json（converter L2262 要求逐段相等）",
            )
            _ensure(
                m["mapping"] == m0["mapping"],
                f"session {api_session} 各 clip 的 mapping 身份不一致",
            )
            _ensure(
                m["target"] == m0["target"],
                f"session {api_session} 各 clip 的 target 不一致",
            )
        local_session_id = m0["session_id"]
        _ensure(
            _SAFE_COMPONENT_RE.match(local_session_id) is not None,
            f"session_id {local_session_id!r} 不是安全路径组件",
        )
        _ensure(
            cat0.get("mode") == CATALOG_MODE_DEV,
            f"session {api_session} catalog mode={cat0.get('mode')!r} ≠ {CATALOG_MODE_DEV}（非 release 档强制）",
        )
        if "provenance" in cat0:
            _ensure(
                cat0["provenance"] in ("compiled", "delivered"),
                f"session {api_session} catalog provenance 越界: {cat0['provenance']}",
            )
        audio_generation = m0.get("audio") is not None
        expected_seg_schema = RECORDING_SCHEMA_V2 if audio_generation else RECORDING_SCHEMA_V1
        for m in metas:
            _ensure(
                m["schema_version"] == expected_seg_schema,
                f"session {api_session} 段 schema {m['schema_version']} ≠ {expected_seg_schema}",
            )
            _ensure(
                (m.get("audio") is not None) == audio_generation,
                f"session {api_session} 音频块跨段不一致（世代由音频唯一决定）",
            )

        # ---- run 结构：api_run ↔ 本地 run_id 双射；run 集合 r0000.. 连续 ----
        run_to_api: dict[str, set] = defaultdict(set)
        clips_by_run: dict[str, list] = defaultdict(list)
        for clip in dump.clips.values():
            rid = clip.meta["run_id"]
            run_to_api[rid].add(clip.api_run)
            clips_by_run[rid].append(clip)
        for rid, apis in run_to_api.items():
            _ensure(
                len(apis) == 1,
                f"本地 run {rid} 映射到多个 API run: {sorted(apis)}",
            )
        api_seen: dict[str, str] = {}
        for rid, apis in run_to_api.items():
            api = next(iter(apis))
            _ensure(
                api not in api_seen,
                f"API run {api} 同时声称本地 run {api_seen.get(api)} 与 {rid}",
            )
            api_seen[api] = rid
        run_ids = sorted(clips_by_run)
        expected_run_ids = [f"r{i:04d}" for i in range(len(run_ids))]
        _ensure(
            run_ids == expected_run_ids,
            f"run 集合不满足 converter 的枚举要求（需 {expected_run_ids}，实得 {run_ids}）",
        )

        # ---- segment 连续性（同 run 内 segment_index 无洞；帧区间逐段/跨 run 连续）----
        mappings = []
        runs_summary = []
        synthesized: list[dict] = []
        expected_frame_start = 0
        prev_qpc_end = None
        run_unknowns: list[dict] = []
        for rid in run_ids:
            run_clips = sorted(clips_by_run[rid], key=lambda c: c.meta["segment_index"])
            indices = [c.meta["segment_index"] for c in run_clips]
            _ensure(
                len(set(indices)) == len(indices),
                f"run {rid} 出现重复 segment_index",
            )
            want = list(range(len(indices)))
            if indices != want:
                gaps = sorted(set(range(indices[-1] + 1)) - set(indices))
                _fail(
                    f"run {rid} 的 segment_index 断裂：实得 {indices[:8]}…，"
                    f"缺口 {gaps}（converter 要求 0..n-1 连续无洞）"
                )
            segs = [c.meta for c in run_clips]
            for a, b in zip(segs, segs[1:]):
                _ensure(
                    b["session_frame_start"] == a["session_frame_start"] + a["n_frames"],
                    f"run {rid} 段 {a['segment_index']}→{b['segment_index']} 帧区间断裂："
                    f"{a['session_frame_start']}+{a['n_frames']} ≠ {b['session_frame_start']}",
                )
                _ensure(
                    b["qpc_start"] >= a["qpc_end"],
                    f"run {rid} 段 {a['segment_index']}→{b['segment_index']} QPC 倒流",
                )
            _ensure(
                segs[0]["session_frame_start"] == expected_frame_start,
                f"run {rid} 起始帧 {segs[0]['session_frame_start']} ≠ 期望 {expected_frame_start}"
                "（跨 run 帧区间必须连续，converter L1940）",
            )
            if prev_qpc_end is not None:
                _ensure(
                    segs[0]["qpc_start"] >= prev_qpc_end,
                    f"run {rid} QPC 相对上一 run 倒流",
                )
            prev_qpc_end = segs[-1]["qpc_end"]
            expected_frame_start += sum(m["n_frames"] for m in segs)

            run_json, unknowns = synthesize_run_json(rid, segs)
            run_unknowns.extend(unknowns)
            run_rel = f"runs/{rid}/run.json"
            run_bytes = _dump_json(run_json)
            synthesized.append(
                {
                    "rel": run_rel,
                    "bytes": len(run_bytes),
                    "sha256": _sha256_bytes(run_bytes),
                    "content": run_bytes,
                    "evidence": f"聚合自 run {rid} 全部 {len(segs)} 个 clip meta",
                }
            )
            # 每段：源对象映射 + sha256.json 合成（= manifest 登记值的平名拷贝）
            for clip in run_clips:
                seg_dir = f"runs/{rid}/segments/{clip.meta['segment_index']:05d}"
                for kind, raw_name in KIND_TO_RAW_NAME.items():
                    if kind not in clip.objects:
                        continue
                    entry = clip.objects[kind]
                    mappings.append(
                        {
                            "key": entry["key"],
                            "rel": f"{seg_dir}/{raw_name}",
                            "size": entry["size"],
                            "sha256": "sha256:" + entry["sha"],
                            "kind": kind,
                        }
                    )
                sha_map = dict(sorted(clip.manifest.items()))
                sha_bytes = _dump_json(sha_map)
                synthesized.append(
                    {
                        "rel": f"{seg_dir}/sha256.json",
                        "bytes": len(sha_bytes),
                        "sha256": _sha256_bytes(sha_bytes),
                        "content": sha_bytes,
                        "evidence": f"clip {clip.clip_id} 的 manifest-*.json 登记值平移",
                    }
                )
            runs_summary.append(
                {
                    "run_id": rid,
                    "api_run": next(iter(run_to_api[rid])),
                    "segment_count": len(segs),
                    "frame_count": sum(m["n_frames"] for m in segs),
                    "session_frame_start": segs[0]["session_frame_start"],
                    "session_frame_end": segs[0]["session_frame_start"]
                    + sum(m["n_frames"] for m in segs),
                    "input_event_count": sum(m["events"]["lines"] for m in segs),
                    "qpc_start": segs[0]["qpc_start"],
                    "qpc_end": segs[-1]["qpc_end"],
                }
            )

        # ---- catalog snapshot：内容寻址找齐 + 交叉引用核验 ----
        snapshots = resolve_snapshots(catalogs_dir, cat0, git_fallback=git_fallback)
        recording_profile = json.loads(snapshots["recording-profile.json"]["bytes"])
        mapping_snapshot = json.loads(snapshots["mapping.json"]["bytes"])
        _ensure(
            (recording_profile.get("audio") is not None) == audio_generation,
            "recording profile 的 audio 声明与段世代不符",
        )
        for name in snapshots:
            synthesized.append(
                {
                    "rel": name,
                    "bytes": len(snapshots[name]["bytes"]),
                    "sha256": snapshots[name]["sha256"],
                    "content": snapshots[name]["bytes"],
                    "evidence": f"仓库注册表逐字节副本（{snapshots[name]['resolved_from']}），"
                    "sha256 与 clip meta catalog 块声明一致",
                }
            )

        # ---- session.json ----
        session_json = synthesize_session_json(m0, recording_profile, mapping_snapshot)
        _ensure(
            session_json["game"]["id"] == mapping_snapshot.get("mapping", mapping_snapshot)["game"]["id"],
            "session.game.id ≠ mapping.json 的 game.id（converter L2140 必拒）",
        )
        session_bytes = _dump_json(session_json)
        synthesized.append(
            {
                "rel": "session.json",
                "bytes": len(session_bytes),
                "sha256": _sha256_bytes(session_bytes),
                "content": session_bytes,
                "evidence": "catalog/contract/mapping/target 块复制自 clip meta；"
                "capture_policy 由 recording-profile 快照按 capture-core 写侧规则推导；"
                "todo 为写侧常量",
            }
        )
        for item in synthesized:
            _assert_red_lines(item["rel"], item["content"])

        # ---- sidecar 归类 ----
        sidecars = [
            {
                "key": s["key"],
                "rel": f"_sidecar/{s['rest']}",
                "size": s["size"],
            }
            for s in dump.sidecars
        ]

        plan_sessions.append(
            {
                "api_session": api_session,
                "session_id": local_session_id,
                "game_id": m0["target"]["game_id"],
                "schema_generation": "v3-audio" if audio_generation else "v2",
                "source_prefix": dump.prefix,
                "mappings": mappings,
                "sidecars": sidecars,
                "synthesized": synthesized,
                "runs": runs_summary,
                "totals": {
                    "clips": len(dump.clips),
                    "mapped_objects": len(mappings),
                    "manifest_objects_consumed": len(dump.clips),
                    "sidecar_objects": len(sidecars),
                    "synthesized_files": len(synthesized),
                    "frames": sum(r["frame_count"] for r in runs_summary),
                },
            }
        )
        provenance_sessions.append(
            {
                "api_session": api_session,
                "session_id": local_session_id,
                "synthesized": [
                    {k: v for k, v in item.items() if k != "content"}
                    for item in synthesized
                ],
                "synthesized_unknown": run_unknowns,
                "writer_constants": [
                    {
                        "file": "session.json",
                        "field": "todo",
                        "value": ["annotator_device_consent_grant", "crash_recovery"],
                        "why": "客户端写侧固定常量（capture-core recording.rs L3162），非会话观测事实",
                    }
                ],
                "catalog_snapshots": [
                    {
                        "name": name,
                        "sha256": snapshots[name]["sha256"],
                        "resolved_from": snapshots[name]["resolved_from"],
                        "declared_by": "clip meta `catalog` 块",
                    }
                    for name in snapshots
                ],
            }
        )

    return {
        "schema": "ingest-remap-plan-1",
        "sessions": plan_sessions,
        "rejected": {
            "sessions": rejected_sessions,
            "clips": [],  # 只做 session 级淘汰：单 clip 越界会污染整个 session 的连续性
        },
        "provenance_sessions": provenance_sessions,
    }


def write_plan(plan: dict, out_dir: Path, game_allowlist: set[str]) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    plan_doc = {
        "schema": plan["schema"],
        "game_allowlist": sorted(game_allowlist),
        "sessions": [
            {**{k: v for k, v in s.items() if k != "synthesized"},
             "synthesized": [
                 {k: v for k, v in item.items() if k != "content"}
                 for item in s["synthesized"]
             ]}
            for s in plan["sessions"]
        ],
        "rejected": plan["rejected"],
    }
    (out_dir / "remap-plan.json").write_bytes(_dump_json(plan_doc))
    provenance = {
        "schema": "ingest-remap-provenance-1",
        "red_lines": {
            "never_writes": ["consented_* 状态", "non_trainable=false", "consent_id"],
            "source": "DataMiner/docs/141 §0-2（93/44 号「回填授权状态」造假禁令）",
        },
        "sessions": plan["provenance_sessions"],
        "rejected": plan["rejected"],
    }
    (out_dir / "PROVENANCE.json").write_bytes(_dump_json(provenance))
    for s in plan["sessions"]:
        base = out_dir / "synthesized" / s["session_id"]
        for item in s["synthesized"]:
            path = base / item["rel"]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(item["content"])


# ---------------------------------------------------------------------------
# emit-manifest（⚠ 网络路径未联网实测；presign=SigV4，凭据仅环境变量，绝不打印）
# ---------------------------------------------------------------------------


def build_mv_manifest(
    plan_doc: dict,
    split: str,
    sign_source_get,
    sign_staged_get,
    puts_by_session: dict | None = None,
    global_puts: dict | None = None,
    include_sidecars: bool = True,
) -> dict:
    """纯函数：plan 文档 + 两个 presign 回调 → mv_pipeline_driver.mjs 的 manifest 形状。

    gets[].rel 即 RAW 相对路径——下载落位就是重命名；合成件按 staged key presign。
    """
    sessions = []
    for s in plan_doc["sessions"]:
        gets = [
            {"url": sign_source_get(m["key"]), "rel": m["rel"], "size": m["size"]}
            for m in s["mappings"]
        ]
        if include_sidecars:
            gets += [
                {"url": sign_source_get(sc["key"]), "rel": sc["rel"], "size": sc["size"]}
                for sc in s["sidecars"]
            ]
        gets += [
            {
                "url": sign_staged_get(s["session_id"], item["rel"]),
                "rel": item["rel"],
                "size": item["bytes"],
            }
            for item in s["synthesized"]
        ]
        sessions.append(
            {
                "session_id": s["session_id"],
                "api_session": s["api_session"],
                "split": split,
                "expect_fail": False,
                "gets": gets,
                "puts": (puts_by_session or {}).get(s["session_id"], {}),
            }
        )
    return {"sessions": sessions, "global_puts": global_puts or {}}


def _make_r2_client(endpoint: str):
    import boto3  # 延迟导入：单测不需要
    from botocore.config import Config

    access_key = os.environ.get("R2_ACCESS_KEY_ID") or os.environ.get("AWS_ACCESS_KEY_ID")
    secret_key = os.environ.get("R2_SECRET_ACCESS_KEY") or os.environ.get(
        "AWS_SECRET_ACCESS_KEY"
    )
    if not access_key or not secret_key:
        _fail(
            "缺 R2 凭据：请设 R2_ACCESS_KEY_ID/R2_SECRET_ACCESS_KEY"
            "（或 AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY）环境变量"
        )
    # R2 必须 SigV4（本目录 README：默认签出的 v2 URL 会 401）
    return boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=access_key,
        aws_secret_access_key=secret_key,
        config=Config(signature_version="s3v4", region_name="auto"),
    )


def cmd_emit_manifest(args) -> int:
    plan_dir = Path(args.plan)
    plan_doc = json.loads((plan_dir / "remap-plan.json").read_text(encoding="utf-8"))
    puts_by_session: dict = {}
    global_puts: dict = {}
    if args.puts_json:
        extra = json.loads(Path(args.puts_json).read_text(encoding="utf-8"))
        puts_by_session = extra.get("sessions", {})
        global_puts = extra.get("global_puts", {})

    staging_prefix = args.staging_prefix.strip("/")

    def staged_key(session_id: str, rel: str) -> str:
        return f"{staging_prefix}/{session_id}/{rel}"

    if args.dry_run:
        counter = {"n": 0}

        def sign_source_get(key: str) -> str:
            counter["n"] += 1
            return f"dry-run://get/{args.source_bucket}/{key}"

        def sign_staged_get(session_id: str, rel: str) -> str:
            return f"dry-run://get/{args.staging_bucket}/{staged_key(session_id, rel)}"
    else:  # pragma: no cover —— 联网路径，未实测（见模块 docstring）
        client = _make_r2_client(args.endpoint)
        # 1) 合成件 PUT 到 staging 前缀
        for s in plan_doc["sessions"]:
            base = plan_dir / "synthesized" / s["session_id"]
            for item in s["synthesized"]:
                body = (base / item["rel"]).read_bytes()
                assert _sha256_bytes(body) == item["sha256"], (
                    f"staging 前重哈希不一致: {item['rel']}"
                )
                client.put_object(
                    Bucket=args.staging_bucket,
                    Key=staged_key(s["session_id"], item["rel"]),
                    Body=body,
                    ContentType="application/json",
                )

        def sign_source_get(key: str) -> str:
            return client.generate_presigned_url(
                "get_object",
                Params={"Bucket": args.source_bucket, "Key": key},
                ExpiresIn=args.expires,
            )

        def sign_staged_get(session_id: str, rel: str) -> str:
            return client.generate_presigned_url(
                "get_object",
                Params={"Bucket": args.staging_bucket, "Key": staged_key(session_id, rel)},
                ExpiresIn=args.expires,
            )

    manifest = build_mv_manifest(
        plan_doc,
        split=args.split,
        sign_source_get=sign_source_get,
        sign_staged_get=sign_staged_get,
        puts_by_session=puts_by_session,
        global_puts=global_puts,
        include_sidecars=not args.no_sidecars,
    )
    out = Path(args.out)
    out.write_bytes(_dump_json(manifest))
    total_gets = sum(len(s["gets"]) for s in manifest["sessions"])
    print(
        f"mv-manifest 写出 {out}: {len(manifest['sessions'])} session / {total_gets} gets"
        + ("（dry-run，URL 为占位符）" if args.dry_run else "")
    )
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def cmd_plan(args) -> int:
    dump_dir = Path(args.session_dump).resolve()
    catalogs_dir = Path(args.catalogs).resolve()
    _ensure(dump_dir.is_dir(), f"--session-dump 不是目录: {dump_dir}")
    _ensure(catalogs_dir.is_dir(), f"--catalogs 不是目录: {catalogs_dir}")
    allowlist = {g.strip() for g in args.game_allowlist.split(",") if g.strip()}
    _ensure(bool(allowlist), "--game-allowlist 不能为空（game 闸是强制项）")

    dumps = load_dump(dump_dir)
    plan = build_plan(
        dumps, catalogs_dir, allowlist, git_fallback=not args.no_git_fallback
    )
    write_plan(plan, Path(args.out), allowlist)

    for r in plan["rejected"]["sessions"]:
        print(
            f"[game 闸] 淘汰 session {r['api_session']}: 见到 {r['games_seen']}，"
            f"越界 {r['games_rejected']}（{r['clip_count']} clips）",
            file=sys.stderr,
        )
    for s in plan["sessions"]:
        t = s["totals"]
        print(
            f"session {s['api_session']} → {s['session_id']} [{s['game_id']}, {s['schema_generation']}]: "
            f"{len(s['runs'])} runs / {t['clips']} segments / {t['frames']} frames；"
            f"映射 {t['mapped_objects']} 对象 + 合成 {t['synthesized_files']} 件 + sidecar {t['sidecar_objects']}"
        )
    if not plan["sessions"]:
        print("没有任何 session 通过 game 闸/一致性检查", file=sys.stderr)
        return 3
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="ingest_remap.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_plan = sub.add_parser("plan", help="产出 remap-plan.json + 合成件 + PROVENANCE.json（纯本地）")
    p_plan.add_argument("--session-dump", required=True, help="_listing.json + 预取 meta/manifest 的目录")
    p_plan.add_argument("--catalogs", required=True, help="仓库 catalog 注册表目录（DataMiner/config/catalogs）")
    p_plan.add_argument("--out", required=True, help="输出目录")
    p_plan.add_argument("--game-allowlist", required=True, help="逗号分隔的 game_id 白名单")
    p_plan.add_argument("--no-git-fallback", action="store_true",
                        help="禁用 git 历史回捞旧版 catalog snapshot（默认启用）")
    p_plan.set_defaults(func=cmd_plan)

    p_emit = sub.add_parser(
        "emit-manifest",
        help="plan → mv-manifest.json（presigned GET/PUT；网络路径未联网实测）",
    )
    p_emit.add_argument("--plan", required=True, help="plan 输出目录")
    p_emit.add_argument("--out", required=True, help="mv-manifest.json 输出路径")
    p_emit.add_argument("--split", default="train")
    p_emit.add_argument("--endpoint", default=os.environ.get("R2_ENDPOINT", ""),
                        help="R2 S3 端点（默认取 R2_ENDPOINT 环境变量）")
    p_emit.add_argument("--source-bucket", required=True, help="源桶（只读，presigned GET）")
    p_emit.add_argument("--staging-bucket", required=True, help="合成件 staging 桶")
    p_emit.add_argument("--staging-prefix", required=True,
                        help="合成件 staging 前缀（如 mv-staging/ingest-remap/<批次>）")
    p_emit.add_argument("--expires", type=int, default=86400, help="presigned URL 有效期（秒）")
    p_emit.add_argument("--puts-json", default=None,
                        help='附加 puts 映射 JSON：{"sessions": {<sid>: {key: url}}, "global_puts": {...}}')
    p_emit.add_argument("--no-sidecars", action="store_true", help="不把 sidecar 对象加进 gets")
    p_emit.add_argument("--dry-run", action="store_true",
                        help="不联网：URL 用占位符，仅验证 manifest 形状")
    p_emit.set_defaults(func=cmd_emit_manifest)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except PlanError as err:
        print(f"错误: {err}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
