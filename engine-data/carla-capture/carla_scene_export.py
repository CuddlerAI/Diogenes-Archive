#!/usr/bin/env python3
"""从 CARLA 世界导出占据图（trajgen `Scene.from_dict` 口径）。

**为什么要在 CARLA 坐标系里规划**：trajgen 烘焙的位姿要被 CARLA 逐帧回放，
中间做一次坐标变换就多一层出错面（docs/70 辩题二「每省一层转换就少一类打回」）。
让 trajgen 直接吃 CARLA 坐标的占据图，回放就是恒等的。

障碍来源：`world.get_environment_objects()` —— CARLA 原生给每个场景物体的
世界 bbox（建筑/围栏/植被/路灯…），比 renderer bounds 更准（它是引擎自己的
语义分类，不含地面与道路）。
可站面来源：`map.get_spawn_points()` + waypoint 网格 —— 车辆能生成/行驶的地方
就是有地面的地方。

用法：
    python3 carla_scene_export.py --out scene.json --center x,y --extent 60
"""

import argparse
import json
import math
import sys

try:
    import carla
except ImportError:
    sys.exit("需要 carla python 包")

#: 当障碍的语义类（正列而非排除——CityObjectLabel 不是标准 enum，
#: 没有 __members__，只能按名字取）。地面/道路/人行道/天空**不算障碍**：
#: 相机与人就在它们上面活动。
OBSTACLE_TAG_NAMES = ("Buildings", "Fences", "Walls", "Poles", "GuardRail",
                      "Vegetation", "TrafficSigns", "TrafficLight", "Bridge",
                      "Static", "Other")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--center", help='"x,y"（世界坐标）；缺省取地图 spawn 点质心')
    ap.add_argument("--extent", type=float, default=60.0, help="半宽（m）")
    ap.add_argument("--height", type=float, default=12.0)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2000)
    args = ap.parse_args()

    client = carla.Client(args.host, args.port)
    client.set_timeout(60.0)
    world = client.get_world()
    cmap = world.get_map()

    spawns = cmap.get_spawn_points()
    if args.center:
        cx, cy = (float(v) for v in args.center.split(","))
    else:
        cx = sum(s.location.x for s in spawns) / len(spawns)
        cy = sum(s.location.y for s in spawns) / len(spawns)

    # 地面高度：中心附近 waypoint 的中位 z
    near = sorted(spawns, key=lambda s: (s.location.x - cx) ** 2 + (s.location.y - cy) ** 2)[:40]
    zs = sorted(s.location.z for s in near)
    ground_z = zs[len(zs) // 2] if zs else 0.0

    e, h = args.extent, args.height
    lo = (-e, 0.0, -e)
    hi = (e, h, e)

    # ---- 障碍：环境物体的世界 bbox（排除地面/道路类）----
    boxes = []
    for name in OBSTACLE_TAG_NAMES:
        label = getattr(carla.CityObjectLabel, name, None)
        if label is None:
            continue
        try:
            objs = world.get_environment_objects(label)
        except RuntimeError:
            continue
        for o in objs:
            b = o.bounding_box
            c, ext = b.location, b.extent
            if abs(c.x - cx) > e + 30 or abs(c.y - cy) > e + 30:
                continue
            blo = (c.x - ext.x - cx, c.z - ext.z - ground_z, c.y - ext.y - cy)
            bhi = (c.x + ext.x - cx, c.z + ext.z - ground_z, c.y + ext.y - cy)
            # CARLA z=up，trajgen 的 y=up：这里就地换轴，导出后全程无转换
            if bhi[1] < 0.1 or blo[1] > h:
                continue
            boxes.append({"lo": [round(v, 3) for v in blo],
                          "hi": [round(v, 3) for v in bhi]})

    # ---- 可站面：**行人导航网格**（navmesh）----
    #
    # 演进了两版，两版都是选址问题：
    #
    # 第一版用 `generate_waypoints` = 机动车道中心线，相机被规划在车道正中，
    # 车流是 autopilot 驱动、不会避让相机 ⇒ cam4 有 5/9 帧穿进公交车厢内部。
    # 占据图只含静态几何，这类穿模规划期躲不开。
    #
    # 第二版改从 driving lane 向右找 Sidewalk 车道。穿模是没了，但 Town10HD 的
    # OpenDRIVE 只在少数路段定义了 sidewalk 车道，**120×120m 的体积里只找出 111 格、
    # 且挤在博物馆边上一小片**——10 条计划的锚点全落在同一个 15×30m 的角落里
    # （x∈[-50,-36]），既缺场景多样性，又有整条贴着柱廊拍、动态物体近乎为零的
    # （clip-03 四路都撞 §8.1「空画面」）。
    #
    # 第三版直接问引擎：**行人能站的地方就是可站面**。`get_random_location_from_navigation`
    # 采样的正是 CARLA 的行人导航网格，覆盖全部人行道与过街道，且与我方
    # 「行人绕锚点布置」用的是同一张网 —— 锚点落在哪，行人就一定能到那，
    # 动态密度不再看运气。人行道车道法作为并集保留（navmesh 偶有缺口）。
    ground_boxes = []
    seen = set()
    NAV_DRAWS = 40000
    NAV_GRID = 2.0
    for _ in range(NAV_DRAWS):
        L = world.get_random_location_from_navigation()
        if L is None or abs(L.x - cx) > e or abs(L.y - cy) > e:
            continue
        key = (round(L.x / NAV_GRID), round(L.y / NAV_GRID))
        if key in seen:
            continue
        seen.add(key)
        half = NAV_GRID * 0.45
        ground_boxes.append({
            "lo": [round(L.x - half - cx, 3), round(L.z - ground_z - 0.2, 3),
                   round(L.y - half - cy, 3)],
            "hi": [round(L.x + half - cx, 3), round(L.z - ground_z + 0.05, 3),
                   round(L.y + half - cy, 3)],
        })
    n_nav = len(ground_boxes)

    step = 4.0
    for wp in cmap.generate_waypoints(step):
        side = wp
        for _ in range(4):                     # driving → shoulder → parking → sidewalk
            nxt = side.get_right_lane()
            if nxt is None:
                break
            side = nxt
            if side.lane_type == carla.LaneType.Sidewalk:
                break
        if side.lane_type != carla.LaneType.Sidewalk:
            continue
        L = side.transform.location
        if abs(L.x - cx) > e or abs(L.y - cy) > e:
            continue
        key = (round(L.x / NAV_GRID), round(L.y / NAV_GRID))   # 与 navmesh 同一张栅格去重
        if key in seen:
            continue
        seen.add(key)
        half = (side.lane_width / 2.0 if side.lane_width else 1.5) * 0.8
        ground_boxes.append({
            "lo": [round(L.x - half - cx, 3), round(L.z - ground_z - 0.2, 3),
                   round(L.y - half - cy, 3)],
            "hi": [round(L.x + half - cx, 3), round(L.z - ground_z + 0.05, 3),
                   round(L.y + half - cy, 3)],
        })

    # ---- 车道栅格：给遮挡判定当「这里一定不是实体」的减项 ----
    #
    # `get_environment_objects` 给的是**轴对齐世界 bbox**，旋转或不规则的楼体、
    # 成片的植被，其 AABB 会横跨街道。实测按它建的遮挡栅格
    # **82.7% 的格子是不透明的**，于是几乎每条视线都被判为挡住，
    # 视锥内可见计数塌到 0.2——数字看起来像「场景里没东西」，其实是判据坏了。
    #
    # 修法不是调 bbox 阈值（p50 才 1.7m、p99 有 29.8m，没有一个干净的分界），
    # 而是拿**行人能站的地方**与**车能开的地方**去减：这两处按定义就不是实体。
    road_cells = []
    seen_r = set()
    for wp in cmap.generate_waypoints(2.0):
        if wp.lane_type != carla.LaneType.Driving:
            continue
        L = wp.transform.location
        if abs(L.x - cx) > e or abs(L.y - cy) > e:
            continue
        key = (round(L.x / 1.5), round(L.y / 1.5))
        if key in seen_r:
            continue
        seen_r.add(key)
        road_cells.append([round(L.x - cx, 2), round(L.y - cy, 2)])

    doc = {
        "name": cmap.name.split("/")[-1],
        "kind": "carla_town",
        "lo": list(lo), "hi": list(hi),
        "ground_y": 0.0,
        "eye_band": [1.4, 2.2],
        "boxes": boxes,
        "ground_boxes": ground_boxes,
        #: 车道格心（局部 x,z）。遮挡判定的减项，见上。
        "road_cells": road_cells,
        # 局部原点的世界坐标：回放时 world = local + offset，**零轴变换**
        "world_offset": [round(cx, 3), round(ground_z, 3), round(cy, 3)],
        "placement": {"yaw_deg": 0, "snap_y": False,
                      "note": "CARLA 坐标系；trajgen 局部坐标 + world_offset 即 CARLA 世界坐标"},
        "export": {"generator": "carla_scene_export", "map": cmap.name,
                   "extent_m": e, "obstacles": len(boxes),
                   "ground_boxes": len(ground_boxes),
                   "ground_from_navmesh": n_nav,
                   "ground_from_sidewalk_lane": len(ground_boxes) - n_nav,
                   "road_cells": len(road_cells)},
    }
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(doc, fh, ensure_ascii=False)
    print("[scene] %s obstacles=%d ground=%d center=(%.1f,%.1f) ground_z=%.2f → %s"
          % (cmap.name, len(boxes), len(ground_boxes), cx, cy, ground_z, args.out))


if __name__ == "__main__":
    main()
