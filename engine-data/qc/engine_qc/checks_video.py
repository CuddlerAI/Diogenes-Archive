"""video 面规则：时长 / 分辨率 / 30fps CFR / H.264 / 音轨（存在 + 响度）。

探测优先 `ffprobe`，缺失时回退 `ffmpeg -i` 解析 stderr——两个都没有才放弃。
回退这一步不是为了省事：mac 上 `brew install ffmpeg` 之外的多数分发（含本仓自带的
静态 ffmpeg）只带 `ffmpeg` 不带 `ffprobe`，而 video 面是 69 号 §8.1 / 68 号 §7 的
第一验收面——整组静默 SKIP 等于把交付验收的一半悄悄关掉。

**没有工具链时不再假装通过**：clip 目录里明明有 mp4 却一个 video 规则都没跑，
判定走 `UNVERIFIED` 而不是 `PASS`（见 report.ClipReport.verdict）。
目录里根本没有 mp4 属于 json-only 的开发自检，仍然 SKIP。

**音轨只查「有流」拦不住静音**：2026-08-07 评审 R2 交付
`2MultiCamera/OasisScene/0807/2026_08_07_shamo_01`，4 路 mp4 都带 aac 流、
volumedetect mean/max 全是 -91.0 dB（数字静音），旧规则照样 PASS——而 69 号
§8.1 视频声音行要求「有声音、声音连续」。故 `video.audio` 在流存在之外必须
用 `ffmpeg -af volumedetect` 实测响度；响度只有 ffmpeg 能测，手里只有 ffprobe
时这一步测不了，按 UNVERIFIED 处理，不降级回存在性检查。
"""

import json
import re
import shutil
import subprocess
from pathlib import Path
from typing import List, Optional, Tuple

from .config import RESOLUTION_WHITELIST, QCConfig
from .report import FAIL, PASS, SKIP, UNVERIFIED, RuleResult

VIDEO_RULES = ["video.duration", "video.resolution", "video.fps_cfr",
               "video.codec", "video.audio"]


def ffprobe_path() -> Optional[str]:
    return shutil.which("ffprobe")


def ffmpeg_path() -> Optional[str]:
    return shutil.which("ffmpeg")


def probe_tool() -> Tuple[Optional[str], Optional[str]]:
    """返回 (工具名, 可执行路径)。都没有则 (None, None)。"""
    p = ffprobe_path()
    if p:
        return "ffprobe", p
    p = ffmpeg_path()
    if p:
        return "ffmpeg", p
    return None, None


def _probe_ffprobe(exe: str, path: Path) -> Optional[dict]:
    cmd = [exe, "-v", "error", "-print_format", "json",
           "-show_streams", "-show_format", str(path)]
    try:
        res = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
        if res.returncode != 0:
            return None
        return json.loads(res.stdout)
    except (subprocess.SubprocessError, json.JSONDecodeError, OSError):
        return None


_DUR_RE = re.compile(r"Duration:\s*(\d+):(\d+):(\d+(?:\.\d+)?)")
_VIDEO_RE = re.compile(r"Stream #\d+:\d+.*?:\s*Video:\s*([A-Za-z0-9_]+)")
_AUDIO_RE = re.compile(r"Stream #\d+:\d+.*?:\s*Audio:\s*([A-Za-z0-9_]+)")
_SIZE_RE = re.compile(r"(?<![\d.])(\d{2,5})x(\d{2,5})(?![\d.])")
_FPS_RE = re.compile(r"([\d.]+)\s*fps")
_TBR_RE = re.compile(r"([\d.]+)\s*tbr")


def _probe_ffmpeg(exe: str, path: Path) -> Optional[dict]:
    """用 `ffmpeg -i` 的 stderr 拼一份 ffprobe 形状的结果。

    映射按 ffmpeg 自己的语义来，不猜：`fps` 是平均帧率（对应 `avg_frame_rate`），
    `tbr` 是流的基准帧率（对应 `r_frame_rate`）——CFR 判据要的正是这两个是否相等。
    """
    try:
        res = subprocess.run([exe, "-hide_banner", "-i", str(path)],
                             capture_output=True, text=True, timeout=60)
    except (subprocess.SubprocessError, OSError):
        return None
    text = (res.stderr or "") + (res.stdout or "")
    if "Duration:" not in text and "Stream #" not in text:
        return None

    duration = None
    m = _DUR_RE.search(text)
    if m:
        duration = int(m.group(1)) * 3600 + int(m.group(2)) * 60 + float(m.group(3))

    streams: List[dict] = []
    for line in text.splitlines():
        vm = _VIDEO_RE.search(line)
        if vm:
            s = {"codec_type": "video", "codec_name": vm.group(1)}
            sm = _SIZE_RE.search(line)
            if sm:
                s["width"], s["height"] = int(sm.group(1)), int(sm.group(2))
            fm, tm = _FPS_RE.search(line), _TBR_RE.search(line)
            if fm:
                s["avg_frame_rate"] = f"{fm.group(1)}/1"
            if tm:
                s["r_frame_rate"] = f"{tm.group(1)}/1"
            if duration is not None:
                s["duration"] = str(duration)
            streams.append(s)
            continue
        am = _AUDIO_RE.search(line)
        if am:
            streams.append({"codec_type": "audio", "codec_name": am.group(1)})
    if not streams:
        return None
    fmt = {"duration": str(duration)} if duration is not None else {}
    return {"streams": streams, "format": fmt}


_MAXVOL_RE = re.compile(r"max_volume:\s*(-?(?:inf|[\d.]+))\s*dB")
_MEANVOL_RE = re.compile(r"mean_volume:\s*(-?(?:inf|[\d.]+))\s*dB")


def _measure_loudness(exe: str, path: Path) -> Optional[Tuple[float, Optional[float]]]:
    """`ffmpeg -af volumedetect` 实测音轨响度，返回 (max_db, mean_db)。

    `-vn` 跳过视频解码——只解音频，60s 片子亚秒级。测不出（ffmpeg 报错 /
    stderr 里没有 max_volume 行）返回 None，由调用方按 UNVERIFIED 处理。
    """
    cmd = [exe, "-hide_banner", "-nostats", "-i", str(path),
           "-vn", "-sn", "-dn", "-af", "volumedetect", "-f", "null", "-"]
    try:
        res = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except (subprocess.SubprocessError, OSError):
        return None
    text = (res.stderr or "") + (res.stdout or "")

    def _db(m):
        if m is None:
            return None
        return float("-inf") if "inf" in m.group(1) else float(m.group(1))

    max_db = _db(_MAXVOL_RE.search(text))
    if max_db is None:
        return None
    return max_db, _db(_MEANVOL_RE.search(text))


def _audio_rule(tool: str, exe: str, path: Path, name: str, via: str,
                astreams: List[dict], cfg: QCConfig) -> RuleResult:
    """video.audio：流存在 + 响度。静音轨（max_volume 低于阈值）判 FAIL。"""
    if not astreams:
        return RuleResult("video.audio", FAIL, name, "无音轨")
    acodec = astreams[0].get("codec_name")
    loud_exe = exe if tool == "ffmpeg" else ffmpeg_path()
    if loud_exe is None:
        return RuleResult("video.audio", UNVERIFIED, name,
                          f"含音轨（{acodec}）但 ffmpeg 不可用，响度未测——不得计 PASS")
    vol = _measure_loudness(loud_exe, path)
    if vol is None:
        return RuleResult("video.audio", UNVERIFIED, name,
                          f"含音轨（{acodec}）但 volumedetect 未返回响度——不得计 PASS")
    max_db, mean_db = vol
    mean_txt = f"，mean_volume {mean_db:.1f} dB" if mean_db is not None else ""
    if max_db < cfg.audio_silence_max_db:
        return RuleResult("video.audio", FAIL, name,
                          f"音轨为静音：max_volume {max_db:.1f} dB{mean_txt}，"
                          f"低于 {cfg.audio_silence_max_db:g} dB（69 号 §8.1 要求有声音）")
    return RuleResult("video.audio", PASS, name,
                      f"含音轨（{acodec}），max_volume {max_db:.1f} dB{mean_txt}{via}")


def _all(status: str, reason: str, target: str = "-") -> List[RuleResult]:
    return [RuleResult(r, status, target, reason) for r in VIDEO_RULES]


def check_videos(files, mode: str, cfg: QCConfig) -> List[RuleResult]:
    present = {k: v for k, v in files.videos.items() if v is not None}
    if not present:
        return _all(SKIP, "无视频文件（video 检查跳过）")
    tool, exe = probe_tool()
    if tool is None:
        # 有 mp4 却没有工具链：这是**未验证**，不是「无需验证」。
        return _all(UNVERIFIED, "ffprobe / ffmpeg 均不可用——video 组未验证，判定不得为 PASS",
                    ",".join(p.name for p in present.values()))

    out: List[RuleResult] = []
    for key in sorted(present):
        path = present[key]
        name = path.name
        info = _probe_ffprobe(exe, path) if tool == "ffprobe" else _probe_ffmpeg(exe, path)
        if info is None:
            out.append(RuleResult("video.codec", FAIL, name, f"{tool} 无法解析该文件"))
            continue
        streams = info.get("streams", [])
        vstreams = [s for s in streams if s.get("codec_type") == "video"]
        astreams = [s for s in streams if s.get("codec_type") == "audio"]
        if not vstreams:
            out.append(RuleResult("video.codec", FAIL, name, "无视频流"))
            continue
        v = vstreams[0]
        via = "" if tool == "ffprobe" else f"（经 {tool} 回退解析）"

        # 时长
        dur = None
        for src in (v.get("duration"), info.get("format", {}).get("duration")):
            try:
                dur = float(src)
                break
            except (TypeError, ValueError):
                continue
        if dur is None:
            out.append(RuleResult("video.duration", FAIL, name, "无法读取时长"))
        else:
            targets = [cfg.mc_duration_s] if mode == "mc" else list(cfg.sfm_durations_s)
            best = min(targets, key=lambda t: abs(dur - t))
            if abs(dur - best) <= cfg.video_dur_tol_s:
                out.append(RuleResult("video.duration", PASS, name,
                                      f"时长 {dur:.2f}s ≈ {best:g}s（±{cfg.video_dur_tol_s}s）{via}"))
            else:
                out.append(RuleResult("video.duration", FAIL, name,
                                      f"时长 {dur:.2f}s 不在允许集 {targets}（±{cfg.video_dur_tol_s}s）"))

        # 分辨率
        w, h = v.get("width"), v.get("height")
        if (w, h) in RESOLUTION_WHITELIST:
            out.append(RuleResult("video.resolution", PASS, name, f"{w}x{h} ∈ 1080P 档位白名单{via}"))
        else:
            out.append(RuleResult("video.resolution", FAIL, name,
                                  f"{w}x{h} ∉ 白名单 {sorted(RESOLUTION_WHITELIST)}"))

        # 30fps CFR（r_frame_rate 与 avg_frame_rate 同为 30/1 作 CFR 代理判据）
        r_rate = v.get("r_frame_rate")
        avg_rate = v.get("avg_frame_rate")

        def _as_fps(s):
            try:
                num, den = s.split("/")
                return float(num) / float(den) if float(den) else None
            except (AttributeError, ValueError, ZeroDivisionError):
                return None

        rf, af = _as_fps(r_rate), _as_fps(avg_rate)
        if rf is not None and af is not None and abs(rf - 30.0) < 0.01 and abs(af - 30.0) < 0.01:
            out.append(RuleResult("video.fps_cfr", PASS, name,
                                  f"r={r_rate} avg={avg_rate}（30fps CFR）{via}"))
        else:
            out.append(RuleResult("video.fps_cfr", FAIL, name,
                                  f"r={r_rate} avg={avg_rate}，应同为 30/1（30fps CFR）"))

        # H.264
        codec = v.get("codec_name")
        if codec == "h264":
            out.append(RuleResult("video.codec", PASS, name, f"codec=h264{via}"))
        else:
            out.append(RuleResult("video.codec", FAIL, name, f"codec={codec}，应为 h264"))

        # 音轨（存在 + 响度，见模块 docstring）
        out.append(_audio_rule(tool, exe, path, name, via, astreams, cfg))
    return out
