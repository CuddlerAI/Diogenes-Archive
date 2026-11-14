"""组级流水线：发现 → 打包面 → JSON 面 → 视频面 → 同步/相机语义 → 运动 → 几何 → 世界线 → 像素。"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Dict, Optional, Tuple

from engine_qc.config import QCConfig

from . import __version__
from .checks_files import check_manifest, check_packaging, check_systeminfo
from .checks_geometry import check_geometry
from .checks_json import LaneData, check_camera_semantics, check_lane_fields, check_take_sync
from .checks_motion import check_motion
from .checks_msync import check_multicam_sync
from .checks_pixels import analyze_pixels
from .checks_video import check_batch_quotas, check_videos
from .checks_worldline import check_worldlines, parse_preroll_s, takeover_frames
from .loaders import discover
from .model import GroupReport, WARN


def check_group(root: Path, expect_s: float = 120.0, roles: Optional[dict] = None, pixels: bool = True,
                n_thumbs: int = 6, cfg: Optional[QCConfig] = None) -> Tuple[GroupReport, dict]:
    t0 = time.time()
    root = Path(root)
    g = discover(root, roles)
    rep = GroupReport(group=g.name, lanes=[lf.info for lf in g.lanes])
    for p in g.problems:
        rep.add("pack.discover", WARN, "group", p, "交付说明")
    rep.extend(check_packaging(g))
    rep.extend(check_manifest(g))

    lanes: Dict[str, LaneData] = {lf.info.key: LaneData(lf) for lf in g.lanes}
    for ld in lanes.values():
        rep.extend(check_lane_fields(ld))
    json_frames = {k: ld.n for k, ld in lanes.items() if ld.records}

    vres, summaries = check_videos(g, expect_s, json_frames)
    rep.extend(vres)
    video_dims = {k: (s["width"], s["height"]) for k, s in summaries.items()}
    rep.extend(check_systeminfo(g, video_dims))
    rep.extend(check_take_sync(g, lanes))
    rep.extend(check_multicam_sync(g))
    rep.extend(check_camera_semantics(g, lanes, video_dims))     # 也把 lane.focal_mm 校成档位
    rep.extend(check_batch_quotas(g, summaries, {lf.info.key: lf.info.focal_mm for lf in g.lanes}))

    tf = takeover_frames(g)
    rep.extend(check_motion(g, lanes, tf, cfg))
    rep.extend(check_geometry(g, lanes, video_dims))
    rep.extend(check_worldlines(g, lanes))
    pre = parse_preroll_s(g.readme)
    pres, art = analyze_pixels(g, lanes, {wl: f / 30.0 for wl, f in tf.items()}, preroll_s=pre,
                               n_thumbs=n_thumbs, enabled=pixels, video_summaries=summaries)
    rep.extend(pres)

    rep.meta.update({
        "tool": f"worldline_qc {__version__}",
        "root": str(root),
        "layout": g.layout,
        "expect_duration_s": expect_s,
        "elapsed_s": round(time.time() - t0, 1),
        "videos": summaries,
        "takeover_frames": tf,
        "preroll_s": pre,
        "readme_present": g.readme is not None,
        "manifest_present": g.manifest is not None,
    })
    rep.lanes = [lf.info for lf in g.lanes]
    return rep, art
