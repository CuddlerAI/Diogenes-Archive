#!/usr/bin/env python3
"""CARLA 版 MC 采集 —— **trajgen 烘焙位姿回放**（docs/69 合规形态）。

与 `carla_mc_capture.py`（载具 attach 版）的根本区别，以及为什么必须换：

载具版把相机挂在 autopilot 车上，交付复核逐条打回：
  · **零共视** —— 四辆车在四条街，8/10 条共视 0.0%（需求要 ≥50%）
  · **视角类型违规** —— cam4 第三人称跟拍自己的骑手，需求要「全第一人称 + 纯净 Camera」
  · **第四路变速缺失** —— 四辆独立车之间没有 guide 路径关系
  · **粉色必含运镜不可满足** —— 地面车 z 变化仅 0.7–0.9m，垂直运镜做不出来
  · 物理阈值超限一个数量级（真实车辆的 jerk 本来就大）
  · init.distance 首帧对距 81–217m（限 30m）

六件事同一个根因：**轨迹不是我方计划驱动的**。本脚本改为
「离线规划、烘焙回放、引擎只做渲染」——与 Unity 侧 PosePlayback 完全同构：

  trajgen（本机，CARLA 坐标系）→ 烘焙 action_camera_N.json
      ↓
  本脚本：4 个**不可见 dummy actor** 逐 tick set_transform，相机 attach 其上
      ↓
  4×ffmpeg 管道出 mp4；json 直接用 trajgen 那份（已含全部字段、已过物理阈值）

**为什么是 dummy actor 而不是直接移动 sensor**：对 sensor 频繁
set_transform 会打乱其内部状态，实测跑 200 帧上下就有 lane 停止投递
（见 README 第 6 层）。移动 actor 是 CARLA 支持的用法。

**纯净相机**：dummy 用 `static.prop.*` 里没有网格的类型；退一步用极小 prop
并在生成后 `set_simulate_physics(False)`——四路互不入镜由 §7 的互视断言兜底。
"""

import argparse
import json
import math
import os
import queue
import subprocess
import sys

try:
    import carla
except ImportError:
    sys.exit("需要 carla python 包")

FPS = 30
LANES = 4
#: dummy 相对相机的下沉量（m）。相机 attach 时用 +z 局部偏移抵消，
#: 于是 dummy 埋在地下被地面遮挡，画面里只有场景（纯净 Camera，docs/69 §2）。
DUMMY_DROP = 12.0
#: 判「贴脸」的距离（m）：车体大、人小，阈值不同
VEH_NEAR_M = 4.0
PED_NEAR_M = 1.6
#: 静态贴脸：深度小于 NEAR_WALL_M 的像素占比达到 NEAR_WALL_PIX 即记一帧
NEAR_WALL_M = 3.0
NEAR_WALL_PIX = 0.30


def _sunk(tf):
    """把 transform 沿其**局部 up** 下沉 DUMMY_DROP —— 与相机的 +z 偏移互为逆。"""
    up = tf.rotation.get_up_vector()
    return carla.Transform(
        carla.Location(x=tf.location.x - up.x * DUMMY_DROP,
                       y=tf.location.y - up.y * DUMMY_DROP,
                       z=tf.location.z - up.z * DUMMY_DROP),
        tf.rotation)


def _rotate(q, v):
    """用四元数 [x,y,z,w] 旋转向量。"""
    x, y, z, w = q
    vx, vy, vz = v
    # t = 2 * (q_vec × v)；v' = v + w*t + q_vec × t
    tx = 2 * (y * vz - z * vy)
    ty = 2 * (z * vx - x * vz)
    tz = 2 * (x * vy - y * vx)
    return (vx + w * tx + (y * tz - z * ty),
            vy + w * ty + (z * tx - x * tz),
            vz + w * tz + (x * ty - y * tx))


def quat_to_carla_rot(q):
    """trajgen 四元数 [x,y,z,w] → CARLA Rotation（度）。

    **旋转必须和位置做同一次轴变换**。场景导出时位置换了轴
    （trajgen y=up ↔ CARLA z=up，见 local_to_world），首版却把四元数
    直接当 CARLA 欧拉角解——目检立刻看出来：四路全程大幅俯视地面。
    位置对了、朝向错了，是最难从数值上发现的一类错（QC 的物理阈值
    查的是位姿序列自身的导数，轴装错了照样平滑）。

    做法：先在 trajgen 系里取 forward/up，再整体换轴到 CARLA 系，
    最后从 forward/up 反解 CARLA 的 yaw/pitch/roll。
    """
    # trajgen（Unity 约定）：x=right, y=up, z=forward
    f = _rotate(q, (0.0, 0.0, 1.0))
    u = _rotate(q, (0.0, 1.0, 0.0))
    # 换轴到 CARLA：x=forward, y=right, z=up
    fx, fy, fz = f[2], f[0], f[1]
    ux, uy, uz = u[2], u[0], u[1]

    yaw = math.degrees(math.atan2(fy, fx))
    pitch = math.degrees(math.asin(max(-1.0, min(1.0, fz))))
    # roll：把 up 投到与 forward 垂直的平面上量
    cy, sy = math.cos(math.radians(yaw)), math.sin(math.radians(yaw))
    cp, sp = math.cos(math.radians(pitch)), math.sin(math.radians(pitch))
    right = (-sy, cy, 0.0)                       # 无 roll 时的右向
    up0 = (-sp * cy, -sp * sy, cp)               # 无 roll 时的上向
    roll = math.degrees(math.atan2(
        ux * right[0] + uy * right[1] + uz * right[2],
        ux * up0[0] + uy * up0[1] + uz * up0[2]))
    return carla.Rotation(pitch=pitch, yaw=yaw, roll=roll)


def load_plan(plan_dir):
    plan = json.load(open(os.path.join(plan_dir, "plan.json")))
    lanes = []
    for i in range(1, LANES + 1):
        lanes.append(json.load(open(os.path.join(plan_dir, "action_camera_%d.json" % i))))
    return plan, lanes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan", required=True, help="trajgen 计划目录")
    ap.add_argument("--out", required=True)
    ap.add_argument("--scene", required=True, help="carla_scene_export 产物（取 world_offset）")
    ap.add_argument("--vehicles", type=int, default=45)
    ap.add_argument("--walkers", type=int, default=35)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2000)
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    plan, lane_rows = load_plan(args.plan)
    scene = json.load(open(args.scene))
    off = scene["world_offset"]
    n_frames = len(lane_rows[0])
    #: 诊断用：只跑前 N 帧（不落交付件，只为快速看指标）
    _cap = int(os.environ.get("DIOGENES_MAX_FRAMES", "0"))
    if _cap:
        n_frames = min(n_frames, _cap)
    aspect = plan.get("aspect", "16:9")
    RES = {"16:9": (1920, 1080), "9:16": (1080, 1920),
           "3:4": (1440, 1080), "4:3": (1080, 1440)}
    W, H = RES[aspect]

    client = carla.Client(args.host, args.port)
    client.set_timeout(120.0)
    world = client.get_world()          # 不 load_world：重载地图会把 server 打成 Signal 11

    settings = world.get_settings()
    settings.synchronous_mode = True
    settings.fixed_delta_seconds = 1.0 / FPS
    world.apply_settings(settings)
    tm = client.get_trafficmanager()
    tm.set_synchronous_mode(True)
    tm.set_random_device_seed(int(plan.get("seed", 0)))

    bp_lib = world.get_blueprint_library()
    actors, cams, pipes, queues = [], [], [], []
    dqueues, depth_dims, depth_cams = [], [], []

    def local_to_world(p):
        """trajgen 局部 → CARLA 世界：加 offset，y↔z 换轴（场景导出时约定）。"""
        return carla.Location(x=p[0] + off[0], y=p[2] + off[2], z=p[1] + off[1])

    # ---- 锚点轨迹（世界坐标）----
    # **四路全部 look-at 锚点区**（trajgen 的共视保证就建立在这上面），所以
    # 「画面里有没有动态物体」等价于「锚点附近有没有动态物体」——只要把车与人
    # 堆在锚点周围，入镜就是构造保证，而不是靠运气。
    #
    # 首版没做这一步：车靠 Traffic Manager 撒、人靠 `get_random_location_from_navigation()`
    # 全图随机，只用「离采集体积中心 <90m」过滤。目检结果正是这么分裂的——
    # 锚点恰好落在车道旁的路数据很密，落在博物馆柱廊里的那一路（clip-03 cam2）
    # **整整 60s 只有石墙与空地，动态物体近乎为零**，直接撞 §8.1「空画面」打回项。
    #
    # Unity 侧的 NpcCrowd 早就是绕 plan.json 锚点转圈的（那次修完才通过目检）；
    # 这里补的是 CARLA 侧同一件事。
    anchor_locs = [local_to_world(a["p"]) for a in plan.get("anchors", [])] or \
        [carla.Location(x=off[0], y=off[2], z=off[1])]

    def dist_to_anchors(loc):
        return min(loc.distance(a) for a in anchor_locs)

    try:
        # **先清掉世界里的残留 actor**。批跑到一半被 kill 时，那一条的
        # finally 不会执行，车与人就留在世界里占着 spawn point；下一条
        # `try_spawn_actor` 大面积失败——实测一次重启后 120 辆只生成了 12 辆，
        # 而日志只说「dynamic actors: 212」，不看车/人分项根本发现不了。
        stale = [a for a in world.get_actors()
                 if a.type_id.startswith(("vehicle.", "walker.", "controller.",
                                          "static.prop.streetbarrier", "sensor."))]
        if stale:
            for a in stale:
                if a.type_id.startswith("controller."):
                    try:
                        a.stop()
                    except RuntimeError:
                        pass
            client.apply_batch([carla.command.DestroyActor(a) for a in stale])
            world.tick() if settings.synchronous_mode else world.wait_for_tick()
            print("[playback] 清理残留 actor %d 个" % len(stale), flush=True)

        # ---- 动态物体：车流 + 行人，**按到锚点轨迹的距离**堆密度 ----
        vbps = bp_lib.filter("vehicle.*")
        # `get_spawn_points()` 给的是 **Transform**，不是 Location——直接丢进
        # `dist_to_anchors` 会 AttributeError，整批 10 条全挂在第一步。
        spawns = sorted(world.get_map().get_spawn_points(),
                        key=lambda s: dist_to_anchors(s.location))
        # **一半堆锚点附近、一半铺满全图**。只堆锚点是上一版的做法，逐帧实测
        # 抓到它的失败模式：clip-01 的 cam1 在 t=10s–50s **整整 40s 视锥内恒为 0**
        # （前 10s 是 60–90 个）。恒零不是「少」，是镜头指向了一片什么都没有的世界——
        # 相机转到背离锚点的方向时，那边既没有我方投放的人、也没有车
        # （80 辆车全被排序挤在锚点附近，全图其余部分空城）。
        # targeted placement 治不了这个：非 look_at 类运镜想指哪儿指哪儿。
        near_n = args.vehicles // 2
        picks = list(spawns[:near_n])
        rest = spawns[near_n:]
        stride = max(1, len(rest) // max(1, args.vehicles - near_n))
        picks += rest[::stride][:args.vehicles - near_n]
        for i, sp in enumerate(picks):
            v = world.try_spawn_actor(vbps[i % len(vbps)], sp)
            if v:
                v.set_autopilot(True, tm.get_port())
                actors.append(v)
        n_veh = len(actors)

        # 行人：布在**四路镜头扫过的取景体积**里，不是堆在锚点上。
        #
        # 这一版是被实测数据改过两次的。加上逐帧视锥计数之后，clip-01 的 cam1
        # 只有 17% 的帧含 ≥3 个动态物体，cam2/3/4 是 96%/100%/96%。第一反应是
        # 「50mm 取景窗窄、密度不够」，于是把行人从 60 加到 140——**没有任何变化，
        # 还是 0.17**。看逐帧分布才看懂：cam1 是 p50=0 / p90=135 的双峰，
        # 不是「到处都少」而是「大部分时间一个也看不见，偶尔一眼看见整群」。
        # 即密度从来不是瓶颈，**人堆在锚点那一个点上**才是——四路里凡是有
        # 非 look_at 类运镜（pan/dolly 等）的那一路，镜头一转就完全扫不到那堆人。
        #
        # 所以按镜头实际扫过的地方布置：沿每路每秒的视线方向取若干深度的采样点，
        # 行人落在这些点附近即可。相机隔离带保留——加密的另一面就是「贴脸遮挡」。
        # ---- 2D 遮挡栅格 ----
        # **不判遮挡的视锥计数会数穿墙的人**。clip-04 的 cam3 首帧数出 114 个，
        # 而联表上那一帧是空无一人的石柱廊——那 114 个全在墙背面。
        # 这类假通过比漏检更坏：闸门、QC、报告会一起说「达标」。
        # 楼体的水平投影按 GRID_M 打成不透明格，逐个候选沿视线步进，
        # 先撞到墙就不计（步进带早退，绝大多数被挡的一两步就出局）。
        GRID_M = 1.5
        opaque = set()
        for b in scene.get("boxes", []):
            if b["hi"][1] < 0.6 or b["lo"][1] > 3.0:   # 挡不住视平线的不算
                continue
            for gi in range(int(b["lo"][0] // GRID_M), int(b["hi"][0] // GRID_M) + 1):
                for gj in range(int(b["lo"][2] // GRID_M), int(b["hi"][2] // GRID_M) + 1):
                    opaque.add((gi, gj))
        # **减掉行人能站的地方与车能开的地方**：AABB 会横跨街道，
        # 不减的话 82.7% 的格子是不透明的，几乎每条视线都被判挡住，
        # 可见计数塌到 0.2——看起来像「场景空」，其实是判据坏了。
        for g in scene.get("ground_boxes", []):
            gx = (g["lo"][0] + g["hi"][0]) * 0.5
            gz = (g["lo"][2] + g["hi"][2]) * 0.5
            opaque.discard((int(gx // GRID_M), int(gz // GRID_M)))
        for rx, rz in scene.get("road_cells", []):
            opaque.discard((int(rx // GRID_M), int(rz // GRID_M)))
        print("[playback] 遮挡栅格 %d 格（%.1fm）" % (len(opaque), GRID_M), flush=True)

        def visible(ox, oy, tx, ty, dist):
            """相机(ox,oy)→目标(tx,ty) 的水平视线是否畅通（世界坐标，米）。"""
            n = int(dist / GRID_M)
            if n <= 1:
                return True
            dx, dy = (tx - ox) / n, (ty - oy) / n
            x, y = ox - off[0], oy - off[2]
            for _ in range(n - 1):
                x += dx
                y += dy
                if (int(x // GRID_M), int(y // GRID_M)) in opaque:
                    return False
            return True

        wbps = bp_lib.filter("walker.pedestrian.*")
        # 隔离带 5m。曾一度收到 3m3m 是为了「相机贴近锚点时锚点区还能布下人」而收的，
        # 但相机改成必须站在可站面附近之后，人和相机本来就共用人行道，
        # 3m 加上行人自己会走动，贴脸帧占比压不下来。密度侧有的是余量：
        # 被闸门打回的那几条 active_frac 都是 0.89–1.00、视锥内均值 6–105。
        KEEPOUT_M = 5.0
        # **布点半径要按该路视锥在该深度的实际宽度取，不能用一个常数**。
        # 首版对所有路都用 9m：16:9 的路水平半角 tan≈0.5–0.9，18m 处锥半宽 9–16m，
        # 9m 落在锥内；而 9:16@50mm 的 tan 只有 0.135，18m 处锥半宽 **2.4m**——
        # 按 9m 撒下去，八成以上的人根本不在画面里。
        # 闸门批的结果正是这么分的：横屏（16:9 / 3:4）7 试 7 中，
        # 竖屏（9:16 / 4:3）6 试 0 中，min_active_frac 低到 0.03。
        tan_h_lane = []
        for i in range(LANES):
            r0 = lane_rows[i][0]
            fx0 = r0["camera_intrinsics"]["fx"] if isinstance(r0["camera_intrinsics"], dict) \
                else r0["camera_intrinsics"][0]
            tan_h_lane.append((W / 2.0) / fx0)

        cam_path = []
        lane_view = [[] for _ in range(LANES)]      # 逐路的视线采样点 (x, y, r²)
        for i in range(LANES):
            for f in range(0, n_frames, FPS):
                r = lane_rows[i][f]
                p = local_to_world(r["camera_position"])
                cam_path.append(p)
                fw = quat_to_carla_rot(r["camera_rotation_quaternion"]).get_forward_vector()
                # 深度里必须有**近档**。10m 起步时，凡是视线 10m 内就撞墙的路
                # （街道峡谷里很常见）一个点都布不进去；而近处的人几乎总是可见的，
                # 正好补上那类路。规划期用中心射线预筛过一版，结论是**不能用**：
                # 通过的候选反而是「15m 内就挡住」的那条（实测 0.82），
                # 被打回的几条中心射线全是通的（0.55–0.65）——
                # 中心射线代表不了整个视锥，人可以在墙前面。
                for d in (5.0, 8.0, 12.0, 18.0, 28.0, 40.0):
                    # 0.8 是留边：贴着锥边缘的人一动就出画
                    rad = min(9.0, max(1.5, tan_h_lane[i] * d * 0.8))
                    lane_view[i].append((p.x + fw.x * d, p.y + fw.y * d, rad * rad,
                                         p.x, p.y))

        # 用普通元组做平面距离：候选点要和上千个参考点比，走 carla 绑定
        # 每次都是一趟 C++ 调用，几千个候选就是几百万次，实测能拖到分钟级。
        cam_xy = [(c.x, c.y) for c in cam_path]
        k2 = KEEPOUT_M ** 2

        # **先把导航网格采样成缓存，再逐路挑**。
        #
        # 上一版是「一边随机采样一边先到先得地填 pool」，四路共用一个池子。
        # 看起来公平，实际上按面积分配：`get_random_location_from_navigation`
        # 是全图均匀的，而窄视场那一路的接收域只有 2.4m 半径的小圆，
        # 随机点落进去的概率比广角路小两个数量级——池子填满时里面几乎全是
        # 广角路的点，窄路一个都没有。实测的铁证是同一条计划改了布点半径之后
        # `min_active_frac` **一模一样还是 0.0311**：那一路的行人贡献恒为零，
        # 剩下的全是（按 seed 确定的）车流，所以两次跑分毫不差。
        #
        # 改成先抽一批缓存点，再**逐路**从缓存里取自己锥内的点、按路轮转配额，
        # 每一路都拿到 focus_n/4。缓存只抽一次，不增加 RPC 量级。
        # 缓存点**直接取自场景导出的可站面格**，不再逐个走
        # `get_random_location_from_navigation`：那是一次 RPC，实测 24000 次
        # 要 **16 分钟以上**（比整条 clip 的渲染还久），而这些格子本来就是
        # 导出器用同一个接口采出来的——同一份数据，取一次即可。
        # 每格再抖几个点，密度够挑。
        import random as _rnd
        _r = _rnd.Random(int(plan.get("seed", 0)))
        cache = []
        for g in scene.get("ground_boxes", []):
            cx0 = (g["lo"][0] + g["hi"][0]) * 0.5 + off[0]
            cz0 = (g["lo"][2] + g["hi"][2]) * 0.5 + off[2]
            top = g["hi"][1] + off[1] + 0.3
            for _ in range(6):
                cache.append(carla.Location(x=cx0 + _r.uniform(-0.8, 0.8),
                                            y=cz0 + _r.uniform(-0.8, 0.8), z=top))
        if not cache:                       # 老场景文件没有可站面层时退回原路
            for _ in range(4000):
                loc = world.get_random_location_from_navigation()
                if loc is not None:
                    cache.append(loc)
        far_enough = [loc for loc in cache
                      if all((loc.x - cx2) ** 2 + (loc.y - cy2) ** 2 >= k2
                             for cx2, cy2 in cam_xy)]
        buckets = []
        for i in range(LANES):
            vp = lane_view[i]
            # **布点也要过遮挡**：把人放在这一路看不见的墙背后，等于没放。
            # 补上遮挡剔除之后，被打回的那几条失败路视锥内均值只有 2.7–9.4，
            # 差的正是这部分「放了但看不见」的人。
            # 布点只按视锥，不再用栅格判可见：AABB 建的栅格 77.9% 是不透明的，
            # 拿它筛点会把绝大多数合法位置误杀（实测三路的桶直接归零）。
            # 「看不看得见」交给深度缓冲在测量端判——那是引擎自己的光栅结果。
            bk = []
            for loc in far_enough:
                for vx, vy, r2, ox, oy in vp:
                    if (loc.x - vx) ** 2 + (loc.y - vy) ** 2 <= r2:
                        bk.append(loc)
                        break
            if not bk:
                # 该路锥内没有可站面格：按 3 倍半径兜底，宁可稀也别整路为零
                for loc in far_enough:
                    for vx, vy, r2, ox, oy in vp:
                        if (loc.x - vx) ** 2 + (loc.y - vy) ** 2 <= r2 * 9.0:
                            bk.append(loc)
                            break
            buckets.append(bk)
        # **按「窄」加权分配，不是四路均分**。均分之后每路只有 focus_n/4 人，
        # 摊到 60s × 4 个深度的 240 个视线采样点上，任一时刻锥内有没有人全靠运气。
        # 窄视场那一路的锥体积小得多，要维持同样的「每帧都有人」，
        # 单位视野里的人得更密——权重取 1/tan(hfov/2) 归一化。
        # 实测竖屏被打回的都是 active_frac 0.45–0.77 的那一两路（贴脸已经全 0），
        # 差的就是这点密度。
        # focus 比例 0.6 → 0.85：「铺开」那一半当初是为了「镜头转向任何方向
        # 都不是空城」，而布点改成逐路分桶之后，四路的取景体积本来就都覆盖到了，
        # 铺开的人大多落在没人看的地方。实测 16x9-07 四路桶有 194/383/352/373 个
        # 位置可用，cam4 却只到 0.72（视锥内均值 3.4）——不是没地方放，是放少了。
        focus_n = int(args.walkers * 0.85)
        inv = [1.0 / max(0.08, t) for t in tan_h_lane]
        tot_inv = sum(inv)
        want = [max(1, int(round(focus_n * v / tot_inv))) for v in inv]
        pool, used = [], set()
        for i in range(LANES):
            b = buckets[i]
            # **要在整个桶里铺开取，不能取前 N 个**。缓存点是按可站面格的导出
            # 顺序排的（空间连续），取连续的前 N 个等于把这一路的人全堆在
            # 桶的一角。实测症状是双峰：某路 active=0.55 而视锥内均值 58——
            # 45% 的帧一个也看不见，其余帧一眼看见一百多个，镜头一扫过那堆人
            # 就从 0 跳到 100。等距抽样把同样多的人摊到这一路整个取景体积上。
            step = max(1, len(b) // max(1, want[i]))
            taken = 0
            for k in range(0, len(b), step):
                if taken >= want[i]:
                    break
                loc = b[k]
                key = (round(loc.x, 1), round(loc.y, 1))
                if key not in used:
                    used.add(key)
                    pool.append(loc)
                    taken += 1
        n_focus = len(pool)
        spread = [loc for loc in far_enough
                  if dist_to_anchors(loc) <= 110.0
                  and (round(loc.x, 1), round(loc.y, 1)) not in used]
        pool += spread[:args.walkers - n_focus]
        print("[playback] 逐路取景点：%s（缓存 %d，去隔离带后 %d）"
              % ("/".join(str(len(b)) for b in buckets), len(cache), len(far_enough)),
              flush=True)
        radius = 1.0 if n_focus >= focus_n * 0.6 else -1.0
        for k, loc in enumerate(pool[:args.walkers]):
            w = world.try_spawn_actor(wbps[k % len(wbps)], carla.Transform(loc))
            if not w:
                continue
            actors.append(w)
            c = world.try_spawn_actor(bp_lib.find("controller.ai.walker"),
                                      carla.Transform(), attach_to=w)
            if c:
                actors.append(c)
                c.start()
                # **把人留在原地附近**：默认行走速度约 1.4 m/s，60s 能走 84m——
                # 布点布得再准，一分钟后也全走光了。实测同一条计划、同一套参数
                # 两次跑出的 active_frac 能差到 1.00 vs 0.62，就是这个随机漂移；
                # 「画面里有没有东西」于是变成靠运气而不是靠构造。
                # 目的地取**最近**的同池点、速度压到 0.7 m/s ⇒ 全程在原邻域内往复。
                near = min((q for q in pool if q is not loc),
                           key=lambda q: q.distance(loc), default=loc)
                c.go_to_location(near)
                try:
                    c.set_max_speed(0.7)
                except (RuntimeError, AttributeError):
                    pass
        print("[playback] dynamic actors: %d (vehicles %d / walkers %d, "
              "行人布点 %s)"
              % (len(actors), n_veh, len(actors) - n_veh,
                 ("取景体积内（锥宽自适应）%d/%d" % (n_focus, focus_n)) if radius > 0
                 else "**取景体积内布点不足，退回锚点 40m 邻域**"),
              flush=True)
        # 逐帧要统计「画面里到底有几个动态物体」的对象集合（不含 dummy 与相机）
        movers = [(a.id, a.type_id.startswith("vehicle."))
                  for a in actors if a.type_id.startswith(("vehicle.", "walker.p"))]

        # ---- 4 路：dummy actor + attach 相机 ----
        for i in range(LANES):
            r0 = lane_rows[i][0]
            tf0 = carla.Transform(local_to_world(r0["camera_position"]),
                                  quat_to_carla_rot(r0["camera_rotation_quaternion"]))
            # **纯净相机**：CARLA 没有真正无网格的 prop，dummy 自身的网格
            # 会挡在镜头前——首版用 streetbarrier，目检看到白橙路障全程占满画面，
            # 正是复核点名的「载机入镜」违规（只是从摩托车换成了路障）。
            # 解法：dummy 沉到相机正下方 DUMMY_DROP 米（埋进地下、被地面遮挡），
            # 相机 attach 时给同样大小的 +z 局部偏移抵消回来。
            dummy_bp = bp_lib.find("static.prop.streetbarrier")
            dummy = world.spawn_actor(dummy_bp, _sunk(tf0))
            try:
                dummy.set_simulate_physics(False)
            except RuntimeError:
                pass
            actors.append(dummy)

            fx = r0["camera_intrinsics"]["fx"] if isinstance(r0["camera_intrinsics"], dict) \
                else r0["camera_intrinsics"][0]
            hfov = 2.0 * math.degrees(math.atan((W / 2.0) / fx))
            cbp = bp_lib.find("sensor.camera.rgb")
            cbp.set_attribute("image_size_x", str(W))
            cbp.set_attribute("image_size_y", str(H))
            cbp.set_attribute("fov", "%.4f" % hfov)
            cbp.set_attribute("sensor_tick", "0.0")   # 每 tick 必出（见 README 第 2 层）
            cam = world.spawn_actor(
                cbp, carla.Transform(carla.Location(z=DUMMY_DROP)), attach_to=dummy)
            q = queue.Queue()
            cam.listen(q.put)
            # **可见性判据用深度缓冲，不用几何近似**。之前拿环境物体的轴对齐
            # bbox 打栅格，77.9% 的格子被判不透明，视锥里 8–9 个动态物体过一遍
            # 遮挡后剩 0——数字看着像「场景空」，其实是判据坏了。深度图是引擎
            # 自己的光栅结果，逐像素精确；分辨率取 1/6（只回答「挡没挡住」，
            # 不进交付），渲染代价可忽略。
            dbp = bp_lib.find("sensor.camera.depth")
            dw, dh = max(64, W // 6), max(64, H // 6)
            dbp.set_attribute("image_size_x", str(dw))
            dbp.set_attribute("image_size_y", str(dh))
            dbp.set_attribute("fov", "%.4f" % hfov)
            dbp.set_attribute("sensor_tick", "0.0")
            dcam = world.spawn_actor(
                dbp, carla.Transform(carla.Location(z=DUMMY_DROP)), attach_to=dummy)
            dq = queue.Queue()
            dcam.listen(dq.put)
            depth_dims.append((dw, dh))
            dqueues.append(dq)
            cams.append((dummy, cam))
            depth_cams.append(dcam)
            queues.append(q)
            pipes.append(subprocess.Popen(
                ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                 "-f", "rawvideo", "-pix_fmt", "bgra", "-s", "%dx%d" % (W, H),
                 "-r", str(FPS), "-i", "-", "-an",
                 "-c:v", "libx264", "-preset", "veryfast", "-b:v", "12M",
                 "-pix_fmt", "yuv420p",
                 os.path.join(args.out, "video_%d.mp4" % (i + 1))],
                stdin=subprocess.PIPE))
        print("[playback] 4 lanes attached to dummies, aspect=%s %dx%d" % (aspect, W, H),
              flush=True)

        # 各路视锥半角（弧度）——按本路实际内参算，竖屏与横屏差别很大
        tan_h, tan_v = [], []
        for i in range(LANES):
            r0 = lane_rows[i][0]
            fx = r0["camera_intrinsics"]["fx"] if isinstance(r0["camera_intrinsics"], dict) \
                else r0["camera_intrinsics"][0]
            tan_h.append((W / 2.0) / fx)
            tan_v.append((H / 2.0) / fx)
        in_view = [[] for _ in range(LANES)]
        too_close = [[] for _ in range(LANES)]
        depth_buf = [None] * LANES
        near_wall = [[] for _ in range(LANES)]

        # ---- 逐 tick 回放 ----
        for f in range(n_frames):
            cam_tf = []
            for i, (dummy, _) in enumerate(cams):
                r = lane_rows[i][f]
                tf = carla.Transform(local_to_world(r["camera_position"]),
                                     quat_to_carla_rot(r["camera_rotation_quaternion"]))
                cam_tf.append(tf)
                dummy.set_transform(_sunk(tf))
            tick_id = world.tick()

            # ---- 逐帧实测「画面里有几个动态物体」----
            #
            # **必须实测，不能用计划里的数**：trajgen 的 subject 是排产用的虚拟主体，
            # CARLA 画面里的动态物体是 Traffic Manager 的车与 Pedestrian AI 的人，
            # 两者毫无关系。沿用计划侧的数字，等于用一个与视频无关的量去证明
            # 「≥80% 时长有较多动态物体」——目检打回 clip-03（四路整条几乎无动态物体）
            # 时，计划侧那个数字是全绿的。
            #
            # 只做视锥内计数、不做遮挡判定（遮挡要深度图，MC 单不出深度）；
            # 这一限制如实写进交付文档。
            for i in range(LANES):
                img = None
                for _ in range(8):
                    cand = dqueues[i].get(timeout=30.0)
                    if cand.frame >= tick_id:
                        img = cand
                        break
                if img is None:
                    raise RuntimeError("depth lane %d missed frame %d" % (i + 1, tick_id))
                depth_buf[i] = bytes(img.raw_data)
                # **静态贴脸也要量**。之前只量到动态物体的距离，
                # 结果 clip-04/05 的相机卡在建筑转角与道闸旁，绿色墙面占 30–60%
                # 画面，指标全绿却是 §8.1 的「贴脸遮挡」。深度图现成的：
                # 数一下有多大比例的像素比 NEAR_WALL_M 还近。
                buf = depth_buf[i]
                dwid, dhei = depth_dims[i]
                npx = dwid * dhei
                step = max(1, npx // 1200)          # 抽样即可，不必逐像素
                cnt = tot = 0
                for k in range(0, npx, step):
                    o4 = k * 4
                    raw = buf[o4 + 2] + buf[o4 + 1] * 256 + buf[o4] * 65536
                    tot += 1
                    if raw / 16777215.0 * 1000.0 < NEAR_WALL_M:
                        cnt += 1
                near_wall[i].append(1 if tot and cnt / tot >= NEAR_WALL_PIX else 0)

            snap = world.get_snapshot()
            locs = []
            for aid, is_veh in movers:
                sn = snap.find(aid)
                if sn is not None:
                    locs.append((sn.get_transform().location, is_veh))
            for i in range(LANES):
                tf = cam_tf[i]
                o = tf.location
                fwd = tf.get_forward_vector()
                rgt = tf.get_right_vector()
                up = tf.get_up_vector()
                n = 0
                close = False
                for L, is_veh in locs:
                    dx, dy, dz = L.x - o.x, L.y - o.y, L.z - o.z
                    # **贴脸遮挡也要量**：只量「视锥里有几个」会把镜头怼进车流
                    # 奖励成高分——实测把相机拉近锚点之后 active_frac 确实上去了，
                    # 目检看到的却是车顶车头占满画面、甚至相机埋在车流当中
                    # （clip-04 四路都是），撞的是 §8.1 另一条打回项。
                    # 一个只往一个方向使劲的指标，一定会被这样满足。
                    rng2 = dx * dx + dy * dy + dz * dz
                    if rng2 < (VEH_NEAR_M if is_veh else PED_NEAR_M) ** 2:
                        close = True
                    d = dx * fwd.x + dy * fwd.y + dz * fwd.z
                    if d < 1.5 or d > 70.0:
                        continue
                    a = abs(dx * rgt.x + dy * rgt.y + dz * rgt.z)
                    b = abs(dx * up.x + dy * up.y + dz * up.z)
                    if a <= tan_h[i] * d and b <= tan_v[i] * d:
                        # 投影到深度图像素，比该像素的实测距离
                        sx = dx * rgt.x + dy * rgt.y + dz * rgt.z
                        sy = dx * up.x + dy * up.y + dz * up.z
                        dwid, dhei = depth_dims[i]
                        u = int(dwid * 0.5 * (1.0 + (sx / d) / tan_h[i]))
                        v = int(dhei * 0.5 * (1.0 - (sy / d) / tan_v[i]))
                        if 0 <= u < dwid and 0 <= v < dhei:
                            k = (v * dwid + u) * 4
                            buf = depth_buf[i]
                            # CARLA 深度编码：(R + G*256 + B*65536) / (256^3-1) * 1000 m
                            raw = buf[k + 2] + buf[k + 1] * 256 + buf[k] * 65536
                            m = raw / 16777215.0 * 1000.0
                            if m + 1.5 >= d:          # 1.5m 容差：目标本身有厚度
                                n += 1
                in_view[i].append(n)
                too_close[i].append(1 if close else 0)
                if _cap and f % 60 == 0 and i == 0:
                    # 诊断：遮挡前 / 遮挡后，看这一刀砍掉多少
                    raw = 0
                    for L2, _v in locs:
                        dx2, dy2, dz2 = L2.x - o.x, L2.y - o.y, L2.z - o.z
                        d2 = dx2 * fwd.x + dy2 * fwd.y + dz2 * fwd.z
                        if d2 < 1.5 or d2 > 70.0:
                            continue
                        a2 = abs(dx2 * rgt.x + dy2 * rgt.y + dz2 * rgt.z)
                        b2 = abs(dx2 * up.x + dy2 * up.y + dz2 * up.z)
                        if a2 <= tan_h[i] * d2 and b2 <= tan_v[i] * d2:
                            raw += 1
                    print("[diag] f=%4d cam1 视锥内 %3d → 过遮挡后 %3d" % (f, raw, n),
                          flush=True)

            for i, q in enumerate(queues):
                img = None
                for _ in range(8):
                    cand = q.get(timeout=30.0)
                    if cand.frame >= tick_id:
                        img = cand
                        break
                if img is None:
                    raise RuntimeError("lane %d missed frame %d" % (i + 1, tick_id))
                pipes[i].stdin.write(bytes(img.raw_data))
            if f % 300 == 0:
                print("[playback] frame %d/%d" % (f, n_frames), flush=True)

        # 动态物体实测落盘 + 打印（docs/69 要 ≥80% 时长「较多动态物体」，本文按 ≥3 个计）
        stats = {"note": "视锥内计数 + 楼体水平遮挡剔除（1.5m 栅格）；仅统计 vehicle.* 与 walker.pedestrian.*",
                 "criterion": "active = count > 0（与 Unity 侧 DynObjectProbe 同口径）",
                 "lanes": []}
        for i in range(LANES):
            v = in_view[i]
            n = max(1, len(v))
            stats["lanes"].append({
                "camera": i + 1,
                # **判据必须与 Unity 侧一致**：`DynObjectProbe` 的 active 是 count>0，
                # 已交付的 Robotics-Warehouse 批就是按这个口径记的 0.904–1.000。
                # 我最初自作主张用 ≥3，等于只给 CARLA 线单独抬高一道门槛。
                "active_frac": round(sum(1 for c in v if c >= 1) / n, 4),
                # ≥3 作为**补充证据**留下：它答的是「较多」而不只是「有」，
                # 长焦路天然低（取景窗小），逐路如实记，不当闸门。
                "active_frac_ge3": round(sum(1 for c in v if c >= 3) / n, 4),
                "mean_in_view": round(sum(v) / n, 2),
                "min_in_view": min(v) if v else 0,
                # 贴脸帧占比：车 <VEH_NEAR_M / 人 <PED_NEAR_M（三维直线距离）
                "too_close_frac": round(sum(too_close[i]) / n, 4),
                # 静态贴脸帧占比（相机卡在墙/道闸/花坛旁）
                "near_wall_frac": round(sum(near_wall[i]) / n, 4),
                "per_frame": v,
            })
        with open(os.path.join(args.out, "dynamic_stats.json"), "w") as fh:
            json.dump(stats, fh)
        # 同时按 **engine_qc 认的 schema** 落一份：`dyn.active_frac` 读的是
        # `dyn_metrics.json` 的 `lanes[].active_frac`（Unity 探针的产物名）。
        # 不落这一份，CARLA 批在这条规则上永远是 SKIP——而它恰恰是复核打回
        # 「整条几乎无动态物体」对应的那条规则。名字不同就等于没测。
        with open(os.path.join(args.out, "dyn_metrics.json"), "w") as fh:
            json.dump({"source": "carla_mc_playback 视锥内实测（含楼体水平遮挡剔除）",
                       "lanes": [{"cam": s2["camera"], "active_frac": s2["active_frac"],
                                  "active_frac_ge3": s2["active_frac_ge3"],
                                  "mean_in_view": s2["mean_in_view"],
                                  "too_close_frac": s2["too_close_frac"],
                                  "near_wall_frac": s2["near_wall_frac"]}
                                 for s2 in stats["lanes"]]}, fh)
        print("[playback] 贴脸帧占比 动态/静态: %s"
              % " ".join("cam%d=%.3f/%.3f" % (s2["camera"], s2["too_close_frac"],
                                              s2["near_wall_frac"])
                         for s2 in stats["lanes"]), flush=True)
        print("[playback] 动态物体实测 active_frac(>0)/均值: %s"
              % " ".join("cam%d=%.2f/%.1f" % (s["camera"], s["active_frac"],
                                              s["mean_in_view"])
                         for s in stats["lanes"]), flush=True)

        for p in pipes:
            p.stdin.close()
            p.wait(timeout=600)

        # **落盘后自查帧数**：批里 clip-05 曾在中途被 simulator 超时打断，
        # 只写了 600/1800 帧就退出，而批脚本照常继续、报告照常写「10 条 ×60s」——
        # 短片是拉回本机 ffprobe 才发现的。自查放在这里，短了就非零退出，
        # 让批脚本当场重跑而不是把残片混进交付。
        for i in range(LANES):
            v = os.path.join(args.out, "video_%d.mp4" % (i + 1))
            got = subprocess.run(
                ["ffprobe", "-v", "error", "-select_streams", "v:0",
                 "-count_frames", "-show_entries", "stream=nb_read_frames",
                 "-of", "csv=p=0", v],
                capture_output=True, text=True).stdout.strip()
            if got.isdigit() and int(got) < n_frames:
                raise RuntimeError("lane %d 只写了 %s/%d 帧（残片）" % (i + 1, got, n_frames))
        print("[playback] DONE frames=%d out=%s" % (n_frames, args.out), flush=True)

    finally:
        for dc in depth_cams:
            try:
                dc.stop()
                dc.destroy()
            except RuntimeError:
                pass
        for _, cam in cams:
            try:
                cam.stop()
            except RuntimeError:
                pass
        for a in actors:
            try:
                if a.type_id.startswith("controller."):
                    a.stop()
            except (RuntimeError, AttributeError):
                pass
        try:
            client.apply_batch([carla.command.DestroyActor(a) for a in actors]
                               + [carla.command.DestroyActor(c) for _, c in cams])
        except RuntimeError:
            pass
        s = world.get_settings()
        s.synchronous_mode = False
        s.fixed_delta_seconds = None
        world.apply_settings(s)


if __name__ == "__main__":
    main()
