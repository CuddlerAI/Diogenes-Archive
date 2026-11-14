"""交付包发现与装载：两种目录布局、README 路表、manifest。

支持的布局：

1. **内部「遍」布局**（PR #959 `stage-group-deliver` 产出）：
   `<group>/{AB,C,D}/{video,systeminfo,action_camera,multicam_sync}_{1..4}.*` + `manifest.json` + `README.md`。
   世界线映射：AB 的 1–3 路 = A，4 路 = B；C、D 各 4 路。
2. **需方平铺布局**（《Multi-camera 12 路相机数据交付说明》）：
   `<group>/{video,systeminfo,action_camera,multicam_sync}_{1..12}.*`（multicam_sync 自交付说明 v1.1，2026-10-16），
   1–3 = A（固定 / 自由 / 自由），4 = B，5–8 = C，9–12 = D（固定 / 自由 / 追背 / 第一人称）。

角色（role）来源优先级：`--roles` 文件 > manifest 的 person_lanes/input_lanes 推断 + README 路表 > 布局默认。
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from .model import LaneInfo

# 平铺布局的固定角色表（交付说明 §五）
FLAT_LAYOUT: Dict[int, Tuple[str, str]] = {
    1: ("A", "fixed"), 2: ("A", "free"), 3: ("A", "free"),
    4: ("B", "free_camera"),
    5: ("C", "fixed"), 6: ("C", "free"), 7: ("C", "chase"), 8: ("C", "first_person"),
    9: ("D", "fixed"), 10: ("D", "free"), 11: ("D", "chase"), 12: ("D", "first_person"),
}
# 「遍」布局的默认角色表（stage_tape 的 roles）
TAKE_LAYOUT: Dict[str, Dict[int, Tuple[str, str]]] = {
    "AB": {1: ("A", "fixed"), 2: ("A", "orbit"), 3: ("A", "dolly"), 4: ("B", "free_camera")},
    "C": {1: ("C", "fixed"), 2: ("C", "orbit"), 3: ("C", "chase"), 4: ("C", "first_person")},
    "D": {1: ("D", "fixed"), 2: ("D", "orbit"), 3: ("D", "chase"), 4: ("D", "first_person")},
}
# 「遍」布局 → 需方 1～12 编号
TAKE_TO_GLOBAL: Dict[str, Dict[int, int]] = {
    "AB": {1: 1, 2: 2, 3: 3, 4: 4},
    "C": {1: 5, 2: 6, 3: 7, 4: 8},
    "D": {1: 9, 2: 10, 3: 11, 4: 12},
}

README_ROLE_WORDS = {
    "固定机位": "fixed", "环绕": "orbit", "横移": "dolly", "自由相机": "free_camera",
    "跟拍": "chase", "第一人称": "first_person", "自由运镜": "free",
}


@dataclass
class LaneFiles:
    info: LaneInfo
    video: Optional[Path]
    systeminfo: Optional[Path]
    action_camera: Optional[Path]
    multicam_sync: Optional[Path] = None   # 交付说明 v1.1（2026-10-16）新增：本路视频下的世界逐帧状态

    def files(self) -> Tuple[Tuple[str, Optional[Path]], ...]:
        return (("video", self.video), ("systeminfo", self.systeminfo), ("action_camera", self.action_camera),
                ("multicam_sync", self.multicam_sync))

    def missing(self) -> List[str]:
        return [name for name, p in self.files() if p is None or not p.is_file()]


@dataclass
class Group:
    root: Path
    layout: str                       # "take" | "flat"
    lanes: List[LaneFiles]
    manifest: Optional[dict] = None
    readme: Optional[str] = None
    problems: List[str] = field(default_factory=list)

    @property
    def name(self) -> str:
        return self.root.name

    def lane(self, take: str, lane: int) -> Optional[LaneFiles]:
        for lf in self.lanes:
            if lf.info.take == take and lf.info.lane == lane:
                return lf
        return None

    def lanes_of(self, worldline: str) -> List[LaneFiles]:
        return [lf for lf in self.lanes if lf.info.worldline == worldline]

    def lanes_in_take(self, take: str) -> List[LaneFiles]:
        return [lf for lf in self.lanes if lf.info.take == take]

    @property
    def takes(self) -> List[str]:
        seen: List[str] = []
        for lf in self.lanes:
            if lf.info.take not in seen:
                seen.append(lf.info.take)
        return seen


def load_json(path: Path):
    """返回 (对象, 错误字符串)。"""
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f), None
    except (OSError, ValueError) as e:
        return None, f"{type(e).__name__}: {e}"


_README_ROW = re.compile(r"^\|\s*(AB|C|D)\s*\|\s*(\d)\s*\|\s*([ABCD])\s*\|\s*([^|]+?)\s*\|\s*([\d.]+)\s*mm\s*\|", re.M)


def parse_readme_lane_table(text: str) -> Dict[Tuple[str, int], Tuple[str, str, float]]:
    """解析交付 README 的「文件夹 | 路 | 世界线 | 相机 | 焦距」表。

    返回 {(take, lane): (worldline, role, focal_mm)}。相机中文 → role 用关键词匹配。"""
    out: Dict[Tuple[str, int], Tuple[str, str, float]] = {}
    for m in _README_ROW.finditer(text):
        take, lane, wl, cam, focal = m.group(1), int(m.group(2)), m.group(3), m.group(4), float(m.group(5))
        role = "free"
        for word, r in README_ROLE_WORDS.items():
            if word in cam:
                role = r
                break
        out[(take, lane)] = (wl, role, focal)
    return out


def _files_for(root: Path, idx: int) -> Tuple[Optional[Path], ...]:
    paths = (root / f"video_{idx}.mp4", root / f"systeminfo_{idx}.json", root / f"action_camera_{idx}.json",
             root / f"multicam_sync_{idx}.json")
    return tuple(p if p.exists() else None for p in paths)


def discover(root: Path, roles_override: Optional[dict] = None) -> Group:
    """识别布局并列出 12 路。缺文件不在这里判 FAIL，只记 problems，由规则层出结论。"""
    root = Path(root)
    manifest, readme = None, None
    problems: List[str] = []
    # 清单和交付说明在包里（manifest.json / README.md），或者像 stage-group-deliver 写的那样在包旁边
    # （<包名>.manifest.json / <包名>.README.md）：两处都找，包里的优先
    mp = root / "manifest.json"
    if not mp.is_file() and (root.parent / f"{root.name}.manifest.json").is_file():
        mp = root.parent / f"{root.name}.manifest.json"
    if mp.is_file():
        manifest, err = load_json(mp)
        if err:
            problems.append(f"manifest.json 不是合法 JSON：{err}")
            manifest = None
    rp = root / "README.md"
    if not rp.is_file() and (root.parent / f"{root.name}.README.md").is_file():
        rp = root.parent / f"{root.name}.README.md"
    if rp.is_file():
        try:
            readme = rp.read_text(encoding="utf-8")
        except OSError as e:
            problems.append(f"README.md 读取失败：{e}")

    take_dirs = [d for d in ("AB", "C", "D") if (root / d).is_dir()]
    flat_present = any((root / f"video_{i}.mp4").exists() or (root / f"action_camera_{i}.json").exists()
                       for i in range(1, 13))
    lanes: List[LaneFiles] = []

    if take_dirs and not flat_present:
        layout = "take"
        table = parse_readme_lane_table(readme) if readme else {}
        mf_wl = (manifest or {}).get("worldlines", {}) if isinstance(manifest, dict) else {}
        for take in ("AB", "C", "D"):
            for lane in range(1, 5):
                wl, role = TAKE_LAYOUT[take][lane]
                focal = None
                if (take, lane) in table:
                    wl, role, focal = table[(take, lane)]
                if isinstance(mf_wl.get(take), dict) and str(lane) in mf_wl[take]:
                    wl = mf_wl[take][str(lane)]
                if roles_override and f"{take}/{lane}" in roles_override:
                    role = roles_override[f"{take}/{lane}"]
                info = LaneInfo(take=take, lane=lane, worldline=wl, role=role, focal_mm=focal,
                                global_index=TAKE_TO_GLOBAL[take][lane])
                found = _files_for(root / take, lane) if (root / take).is_dir() else (None,) * 4
                lanes.append(LaneFiles(info, *found))
    else:
        layout = "flat"
        for idx in range(1, 13):
            wl, role = FLAT_LAYOUT[idx]
            if roles_override and str(idx) in roles_override:
                role = roles_override[str(idx)]
            info = LaneInfo(take="flat", lane=idx, worldline=wl, role=role, focal_mm=None, global_index=idx)
            lanes.append(LaneFiles(info, *_files_for(root, idx)))
        if take_dirs:
            problems.append("同时存在平铺文件与 AB/C/D 子目录，按平铺布局处理")

    return Group(root=root, layout=layout, lanes=lanes, manifest=manifest, readme=readme, problems=problems)
