"""次级文件夹（clip）文件发现与 JSON 装载。

命名规范：
- MC（docs/71 §8 / delivery README）：12 文件
  `video_N.mp4 + systeminfo_N.json + action_camera_N.json`（N=1..4）
- SFM（docs/68 §8 镜像）：`video.mp4 + systeminfo.json +
  action_camera_30fps.json + action_camera_15fps.json (+ depth.zip)`
- 可选内部元数据：`plan.json`（trajgen 计划）、`covis.json`（排程器共视产物）
"""

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Optional


@dataclass
class ClipFiles:
    root: Path
    mode: str
    action: Dict[str, Optional[Path]] = field(default_factory=dict)
    sysinfo: Dict[str, Optional[Path]] = field(default_factory=dict)
    videos: Dict[str, Optional[Path]] = field(default_factory=dict)
    depth_zip: Optional[Path] = None
    plan: Optional[Path] = None
    covis: Optional[Path] = None


def _opt(path: Path) -> Optional[Path]:
    return path if path.is_file() else None


def discover(root: Path, mode: str) -> ClipFiles:
    root = Path(root)
    cf = ClipFiles(root=root, mode=mode)
    if mode == "mc":
        for n in range(1, 5):
            key = str(n)
            cf.action[key] = _opt(root / f"action_camera_{n}.json")
            cf.sysinfo[key] = _opt(root / f"systeminfo_{n}.json")
            cf.videos[key] = _opt(root / f"video_{n}.mp4")
    else:  # sfm
        cf.action["30fps"] = _opt(root / "action_camera_30fps.json")
        cf.action["15fps"] = _opt(root / "action_camera_15fps.json")
        cf.sysinfo["main"] = _opt(root / "systeminfo.json")
        video = _opt(root / "video.mp4")
        if video is None:
            mp4s = sorted(root.glob("*.mp4"))
            if len(mp4s) == 1:
                video = mp4s[0]
        cf.videos["main"] = video
        cf.depth_zip = _opt(root / "depth.zip")
    cf.plan = _opt(root / "plan.json")
    cf.covis = _opt(root / "covis.json")
    return cf


def load_json(path: Path):
    """返回 (obj, err)。err 非 None 时 obj 为 None。"""
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f), None
    except (json.JSONDecodeError, UnicodeDecodeError, OSError) as e:
        return None, f"{type(e).__name__}: {e}"


def discover_batch_clips(batch_root: Path):
    """批目录下递归发现 clip 目录：含任一 action_camera*.json 的目录。"""
    batch_root = Path(batch_root)
    clips = set()
    for p in batch_root.rglob("action_camera*.json"):
        clips.add(p.parent)
    return sorted(clips)
