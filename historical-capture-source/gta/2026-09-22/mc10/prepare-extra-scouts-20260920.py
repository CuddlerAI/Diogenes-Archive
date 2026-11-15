"""Prepare two offline scout proposals; never visits or records the game."""
import copy
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys

BASE = Path(__file__).resolve().parent
CAPTURE = BASE.parents[1]
sys.path.insert(0, str(CAPTURE))
from gta_capture.scenario import Scenario

OUT = BASE / "review/extra-corridor-scout-20260920-01"
assert not OUT.exists(), "Preserve existing scout proposal."
template = json.loads((BASE / "review/open-corridor-scout-20260920-01/03-senora-desert/config/mid-scenario.json").read_text(encoding="utf8"))
commit = "1aeb099148355bcbe463924104969efa6e6c0623"
specs = [
    ("01-davis-quartz", "21-davis-quartz.json", 0, 1, "14:00", "Two adjacent published points on one quarry level; rock piles, bends, vehicles and actual ground height remain unverified."),
    ("02-grapeseed-farmland", "18-grapeseed.json", 15, 16, "09:00", "Adjacent published points near farmland; road traffic, fences, crops and actual ground height remain unverified."),
]
entries = []
for scene_index, (name, filename, ia, ib, clock, caveat) in enumerate(specs):
    api = f"repos/gtbikev/courses/contents/courses/{filename}?ref={commit}"
    raw = subprocess.run(["gh", "api", api, "-H", "Accept: application/vnd.github.raw+json"], capture_output=True, check=True).stdout
    source = json.loads(raw)
    a, b = (source["WayPointList"][i] for i in (ia, ib))
    dx, dy = b["X"] - a["X"], b["Y"] - a["Y"]
    length = math.hypot(dx, dy)
    heading = math.degrees(math.atan2(-dx, dy)) % 360
    center = [(a[k] + b[k]) / 2 for k in ("X", "Y", "Z")]
    records = []
    for point_index, (label, offset) in enumerate((("start", -50), ("quarter", -25), ("mid", 0), ("threequarter", 25), ("end", 50))):
        scenario = copy.deepcopy(template)
        scenario["scenario_id"] = f"gta-mc10-extra-{name}-{label}-scout-20260920-01"
        scenario["seed"] = 202609203001 + 10 * scene_index + point_index
        scenario["clock"] = clock
        scenario["location"] = [center[0] + dx / length * offset, center[1] + dy / length * offset, center[2]]
        scenario["player"]["heading_deg"] = heading
        path = OUT / name / "config" / f"{label}-scenario.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(scenario, ensure_ascii=False, indent=2) + "\n", encoding="utf8")
        Scenario.load(path)
        records.append({"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "location": scenario["location"]})
    entries.append({"name": name, "source_url": f"https://raw.githubusercontent.com/gtbikev/courses/{commit}/courses/{filename}", "source_bytes": len(raw), "source_sha256": hashlib.sha256(raw).hexdigest(), "source_waypoint_indexes_zero_based": [ia, ib], "source_points": [a, b], "source_chord_m": length, "center": center, "heading_deg": heading, "derivation": "100-unit XY chord centered between the two published points; copied average Z is only a teleport request guess.", "risk": caveat, "scenario_files": records})
receipt = {"schema": "dm-gta-offline-scout-proposals/1", "status": "prepared_not_visited", "sources_are_primary_author_route_data": True, "game_calls_performed": False, "downloaded_code_executed": False, "props_spawned": False, "clearance_or_ground_verified": False, "entries": entries}
(OUT / "plan-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf8")
(OUT / "README.md").write_text("# 额外场景探索候选\n\n两组配置已通过 Scenario.load，但尚未在游戏访问。优先使用已准备的跑马场、西码头、Senora；本组作为采石场和农田的后续备选。\n\n每组先访问 mid，再查看 start/quarter/threequarter/end。坐标来自作者路线的相邻导航点，只能定位探索，不能证明步行线路、地面高度或侧机位净空。实际落点不稳定、遇到任务/围栏/石堆/车流时，应保留访问回执并弃用或重新勘景。所有访问与录制必须使用新的独立证据路径。\n\n公开原始来源、精确点位、推导及输入摘要见 plan-receipt.json；未安装或执行外部mod，未生成任何Props。\n", encoding="utf8")
print(json.dumps({"out": str(OUT), "scenarios_valid": 10, "game_calls_performed": False}, ensure_ascii=False))
