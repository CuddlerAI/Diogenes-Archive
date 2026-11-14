"""测试共用：sys.path 引导 + 造一个采集目录。"""

import json
import sys
from pathlib import Path

DELIVERY_ROOT = Path(__file__).resolve().parents[1]  # engine-data/delivery
if str(DELIVERY_ROOT) not in sys.path:
    sys.path.insert(0, str(DELIVERY_ROOT))

from engine_delivery.spec import MC_INTERNAL_FILES, MC_REQUIRED_FILES  # noqa: E402

SYSINFO = {"gameProcessName": "MegacityMetro", "x": 0, "y": 0,
           "width": 1920, "height": 1080, "recordDpi": 1.0}


def make_capture_dir(root: Path, with_internal=True, drop=()):
    """造一个采集出的次级文件夹：12 必要文件 + 我方内部产物。"""
    root.mkdir(parents=True, exist_ok=True)
    for name in MC_REQUIRED_FILES:
        if name in drop:
            continue
        if name.endswith(".mp4"):
            (root / name).write_bytes(b"\x00" * 32)
        elif name.startswith("systeminfo"):
            (root / name).write_text(json.dumps(SYSINFO), encoding="utf-8")
        else:
            (root / name).write_text("[]", encoding="utf-8")
    if with_internal:
        for name in MC_INTERNAL_FILES:
            if name.endswith(".wav"):
                (root / name).write_bytes(b"\x00" * 8)
            else:
                (root / name).write_text("{}", encoding="utf-8")
    return root


def depth_rows(seqs, extra=None):
    return [dict({"present_seq": s, "width": 1280, "height": 720,
                  "format": "d32_float_s8_x24", "dsv_id": "main-scene-v1",
                  "path": f"raw/{s:012d}.f32"}, **(extra or {})) for s in seqs]
