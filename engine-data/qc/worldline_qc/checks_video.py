"""视频容器面：时长/帧数、CFR 30fps、分辨率与画幅、编码、音轨。ffprobe 探测复用 engine_qc.checks_video。

需求出处：新需求 §3.2「文件与时长 / 分辨率与画幅 / 焦距与FOV」、§3.3 音频、§7.1。
"""

from __future__ import annotations

import math
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from engine_qc.checks_video import _measure_loudness, _probe_ffmpeg, _probe_ffprobe, ffmpeg_path, ffprobe_path

from .loaders import Group
from .model import FAIL, MANUAL, PASS, SKIP, UNVERIFIED, WARN, RuleResult

# 画幅白名单：(宽高比, 最低宽, 最低高)
ASPECTS = {
    "16:9": (16 / 9, 1920, 1080),
    "9:16": (9 / 16, 1080, 1920),
    "4:3": (4 / 3, 1440, 1080),
    "3:4": (3 / 4, 1080, 1440),
}


def probe(path: Path) -> Optional[dict]:
    exe = ffprobe_path()
    if exe:
        return _probe_ffprobe(exe, path)
    exe = ffmpeg_path()
    if exe:
        return _probe_ffmpeg(exe, path)
    return None


def _rate(s: str) -> Optional[float]:
    try:
        n, d = s.split("/")
        return float(n) / float(d) if float(d) else None
    except (ValueError, AttributeError):
        return None


def classify_aspect(w: int, h: int) -> Optional[str]:
    for name, (ratio, mw, mh) in ASPECTS.items():
        if abs(w / h - ratio) < 0.01 and w >= mw and h >= mh:
            return name
    return None


def video_summary(path: Path) -> Optional[dict]:
    """把 ffprobe 结果压成一层字典；None = 探不到。"""
    info = probe(path)
    if not info:
        return None
    v = next((s for s in info.get("streams", []) if s.get("codec_type") == "video"), None)
    a = next((s for s in info.get("streams", []) if s.get("codec_type") == "audio"), None)
    if v is None:
        return None
    dur = v.get("duration") or info.get("format", {}).get("duration")
    nb = v.get("nb_frames")
    return {
        "codec": v.get("codec_name"),
        "width": int(v.get("width", 0)), "height": int(v.get("height", 0)),
        "r_frame_rate": _rate(v.get("r_frame_rate", "")), "avg_frame_rate": _rate(v.get("avg_frame_rate", "")),
        "duration": float(dur) if dur not in (None, "N/A") else None,
        "nb_frames": int(nb) if nb not in (None, "N/A") else None,
        "audio_codec": a.get("codec_name") if a else None,
        "audio_rate": int(a.get("sample_rate", 0)) if a else None,
        "audio_channels": int(a.get("channels", 0)) if a else None,
    }


def check_videos(g: Group, expect_s: float, json_frames: Dict[str, int]) -> Tuple[List[RuleResult], Dict[str, dict]]:
    """返回 (规则结果, {lane_key: summary})。expect_s 为需求时长（三期 120 s）。"""
    out: List[RuleResult] = []
    summaries: Dict[str, dict] = {}
    have_tool = bool(ffprobe_path() or ffmpeg_path())
    for lf in g.lanes:
        key = lf.info.key
        if lf.video is None:
            out.append(RuleResult("video.probe", SKIP, key, "视频缺失（见 pack.files）", "需求§3.2"))
            continue
        if not have_tool:
            out.append(RuleResult("video.probe", UNVERIFIED, key, "本机无 ffprobe/ffmpeg，视频参数未核验", "需求§3.2"))
            continue
        s = video_summary(lf.video)
        if s is None:
            out.append(RuleResult("video.probe", UNVERIFIED, key, "ffprobe 无法解析该文件", "需求§3.2"))
            continue
        summaries[key] = s
        # 编码 / 容器
        if s["codec"] == "h264":
            out.append(RuleResult("video.codec", PASS, key, "MP4/H.264，可正常解码", "需求§3.2/§7.1"))
        else:
            out.append(RuleResult("video.codec", WARN, key, f"视频编码 {s['codec']}（需求只写 MP4，H.264 是一期口径）", "需求§3.2"))
        # 帧率 CFR 30
        r, avg = s["r_frame_rate"], s["avg_frame_rate"]
        if r and avg and abs(r - 30) < 1e-6 and abs(avg - 30) < 1e-3:
            out.append(RuleResult("video.fps_cfr", PASS, key, f"r={r:g} avg={avg:.3f}，固定 30fps", "需求§3.2"))
        else:
            out.append(RuleResult("video.fps_cfr", FAIL, key, f"r={r} avg={avg}，应固定 30fps（禁止动态帧率）", "需求§3.2"))
        # 时长 / 帧数
        n_expect = int(round(expect_s * 30))
        nb, dur = s["nb_frames"], s["duration"]
        if nb is not None and nb == n_expect and dur is not None and abs(dur - expect_s) <= 0.05:
            out.append(RuleResult("video.duration", PASS, key, f"{nb} 帧 / {dur:.3f} s（要求 {n_expect} 帧 / {expect_s:g} s）", "需求§3.2/§7.1"))
        else:
            out.append(RuleResult("video.duration", FAIL, key,
                                  f"{nb} 帧 / {dur if dur is None else round(dur, 3)} s，需求为每路 {expect_s:g} 秒、{n_expect} 帧", "需求§3.2/§7.1"))
        # 视频帧数 == JSON 记录数
        jn = json_frames.get(key)
        if jn is None:
            out.append(RuleResult("video.json_frames", SKIP, key, "无 action_camera 记录数可比", "需求§5.4/§6.2"))
        elif nb == jn:
            out.append(RuleResult("video.json_frames", PASS, key, f"视频 {nb} 帧 = action_camera {jn} 条记录", "需求§5.4/§6.2"))
        else:
            out.append(RuleResult("video.json_frames", FAIL, key, f"视频 {nb} 帧 ≠ action_camera {jn} 条记录", "需求§5.4/§6.2"))
        # 分辨率与画幅
        w, h = s["width"], s["height"]
        asp = classify_aspect(w, h)
        if asp and min(w, h) >= 1080:
            out.append(RuleResult("video.resolution", PASS, key, f"{w}×{h}（{asp}），宽高均 ≥1080", "需求§3.2"))
        else:
            out.append(RuleResult("video.resolution", FAIL, key,
                                  f"{w}×{h} 不在允许画幅（16:9 1920×1080 / 9:16 1080×1920 / 4:3 ≥1440×1080 / 3:4 ≥1080×1440）", "需求§3.2"))
        # 音轨
        if s["audio_codec"] is None:
            out.append(RuleResult("video.audio", FAIL, key, "无音轨（需求：视频须有与场景事件对应的原声，不得无声）", "需求§3.3/§6.6/§7.1"))
        else:
            exe = ffmpeg_path()
            loud = _measure_loudness(exe, lf.video) if exe else None
            if loud is None:
                out.append(RuleResult("video.audio", UNVERIFIED, key, f"音轨 {s['audio_codec']} {s['audio_rate']}Hz×{s['audio_channels']}，响度未能实测", "需求§3.3/§6.6"))
            else:
                mx, mean = loud
                if mx is not None and mx > -70:
                    out.append(RuleResult("video.audio", PASS, key,
                                          f"音轨 {s['audio_codec']} {s['audio_rate']}Hz×{s['audio_channels']}，max {mx:.1f} dB / mean {mean if mean is None else round(mean, 1)} dB",
                                          "需求§3.3/§6.6", data={"max_db": mx, "mean_db": mean}))
                else:
                    out.append(RuleResult("video.audio", FAIL, key, f"音轨近乎静音（max {mx} dB）", "需求§3.3/§6.6"))
    # BGM / 音画同步 / 方位：人工
    if summaries:
        out.append(RuleResult("video.audio_content", MANUAL, "group",
                              "音轨是否含游戏伴奏 BGM、是否与本世界线事件同步、C/D 干预后音频是否对应改变后事件、多路方位口径——须人工试听",
                              "需求§3.3/§6.6"))
    return out, summaries


def check_batch_quotas(g: Group, summaries: Dict[str, dict], focal_by_lane: Dict[str, Optional[float]]) -> List[RuleResult]:
    """画幅 / 焦距配额是批次层面要求，单组只报分布。"""
    out: List[RuleResult] = []
    if summaries:
        asp = {}
        for key, s in summaries.items():
            a = classify_aspect(s["width"], s["height"]) or f"{s['width']}×{s['height']}"
            asp[a] = asp.get(a, 0) + 1
        out.append(RuleResult("quota.aspect", WARN, "group",
                              f"本组画幅分布 {asp}；批次要求 16:9 60% / 9:16 20% / 4:3 10% / 3:4 10%（单组不判定，按批次统计；一组 12 路是否统一画幅待需方确认）",
                              "需求§3.2", data={"aspect": asp}))
    focals = {}
    for key, f in focal_by_lane.items():
        if f is None:
            continue
        focals[f"{f:g}mm"] = focals.get(f"{f:g}mm", 0) + 1
    if focals:
        off = [k for k in focals if k not in ("18mm", "24mm", "35mm", "50mm")]
        st = FAIL if off else WARN
        out.append(RuleResult("quota.focal", st, "group",
                              f"本组焦距分布 {focals}；批次要求 18/24/35/50mm 各 25%（单组不判定）"
                              + (f"；出现非档位焦距 {off}" if off else ""), "需求§3.2", data={"focal": focals}))
    return out
