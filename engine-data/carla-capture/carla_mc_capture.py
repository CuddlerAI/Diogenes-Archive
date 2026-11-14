#!/usr/bin/env python3
"""CARLA 版 Multi-Camera 采集（docs/69 交付形态）。

**为什么 CARLA 不走 Unity SDK 那条路**：我们在 Unity 侧费力实现的东西——
4 路同帧、view-space 线性深度、逐帧相机位姿——CARLA 全是原生能力
（它本来就是为生成带真值的合成数据设计的）。所以这一侧不移植 SDK，
只做三件事：轨迹驱动、按 docs/69 schema 落盘、对接 engine_qc。

许可（docs/96 §1，🟩 README 原文核实）：
    "CARLA specific code is distributed under MIT License."
    "CARLA specific assets are distributed under CC-BY License."
⇒ **渲染输出可对外交付**，义务是 CC-BY 署名传递进交付文档。

同帧保证：`synchronous_mode=True` + `fixed_delta_seconds=1/30` + `world.tick()`——
所有传感器在同一 tick 出数据，帧对齐由构造保证（与 Unity 侧 FrameClock 同思路）。

用法：
    python3 carla_mc_capture.py --out <dir> --seed 1 --town Town10HD_Opt \
        [--duration 60] [--vehicles 60] [--walkers 40]
"""

import argparse
import json
import math
import os
import queue
import random
import subprocess
import sys

try:
    import carla
except ImportError:
    sys.exit("需要 carla python 包：把 CARLA 的 PythonAPI 加进 PYTHONPATH，"
             "或 pip install carla==0.9.15")

FPS = 30
LANES = 4
#: docs/69 画幅与焦距配额
ASPECT_RES = {"16:9": (1920, 1080), "9:16": (1080, 1920),
              "3:4": (1440, 1080), "4:3": (1080, 1440)}
FOCALS = (18, 24, 35, 50)


def fov_from_focal(focal_mm: float, width: int, height: int) -> float:
    """CARLA 的 `fov` 是**水平**视场角（度）。

    我方按 35mm 等效焦距排产，纵向等效：fy = f·H/24 ⇒
    tan(hfov/2) = (W/2)/fy = 12·(W/H)/f。竖屏水平视场因此比横屏窄得多
    （9:16@24mm 只有 31°，16:9 是 83°）——这与 Unity 侧同一个公式，
    两边必须一致，否则同一份计划在两条产线上画幅语义不同。
    """
    return 2.0 * math.degrees(math.atan(12.0 * (width / height) / focal_mm))


class LaneRig:
    """一路：RGB 传感器 + 一条 ffmpeg 管道。"""

    def __init__(self, world, bp_lib, idx, focal, aspect, out_dir):
        w, h = ASPECT_RES[aspect]
        self.idx = idx
        self.w, self.h = w, h
        self.focal = focal
        fov = fov_from_focal(focal, w, h)
        self.fov = fov

        def mk(bp_name):
            bp = bp_lib.find(bp_name)
            bp.set_attribute("image_size_x", str(w))
            bp.set_attribute("image_size_y", str(h))
            bp.set_attribute("fov", "%.4f" % fov)
            # **sensor_tick 必须留 0（每 tick 必出）**：设成 1/30 与
            # fixed_delta_seconds 相同看似合理，但 CARLA 把它当「最小间隔」，
            # 浮点误差会让某些 tick 不出数据 ⇒ 队列 get 超时。
            # 实测：frame 0 正常、frame 1 即挂，且异常被 finally 里
            # destroy 已死 actor 的 C++ terminate 掩盖，报错完全指错方向。
            bp.set_attribute("sensor_tick", "0.0")
            return bp

        # **只挂 RGB**：docs/69 的 MC 交付物是 12 文件
        # （video/action_camera/systeminfo 各 4 路），**不含 depth**——
        # depth 是 docs/68 SFM 单的要求，两单别混。多挂一路 depth 让写盘量翻倍，
        # 而它根本不进 MC 交付面。
        self.rgb_bp = mk("sensor.camera.rgb")
        self.rgb = None
        self.rgb_q = queue.Queue()
        self.proc = None
        self.frames = []
        self.out_dir = out_dir

    def spawn(self, world, transform, parent=None):
        self.rgb = world.spawn_actor(self.rgb_bp, transform, attach_to=parent)
        self.rgb.listen(self.rgb_q.put)
        # ffmpeg 管道：raw BGRA → stdin，与 Unity 侧 FfmpegPipe 同架构。
        # **不能用 save_to_disk**——它逐帧做 PNG 编码，实测吞吐 0.13 fps
        # （一条 60s clip 要 3.7 小时），瓶颈在 CPU 编码而不是渲染（GPU 才 0%）。
        os.makedirs(self.out_dir, exist_ok=True)
        self.proc = subprocess.Popen(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
             "-f", "rawvideo", "-pix_fmt", "bgra",
             "-s", "%dx%d" % (self.w, self.h), "-r", str(FPS), "-i", "-",
             "-an", "-c:v", "libx264", "-preset", "veryfast", "-b:v", "12M",
             "-pix_fmt", "yuv420p",
             os.path.join(self.out_dir, "video_%d.mp4" % (self.idx + 1))],
            stdin=subprocess.PIPE)

    def write_frame(self, image):
        self.proc.stdin.write(bytes(image.raw_data))

    def finish(self):
        if self.proc is not None and self.proc.stdin:
            self.proc.stdin.close()
            self.proc.wait(timeout=300)

    def set_transform(self, tf):
        self.rgb.set_transform(tf)

    def intrinsics(self):
        """按 docs/69：fx=fy=(H/2)/tan(vFOV/2)，cx=W/2, cy=H/2。

        CARLA 给的是水平 fov，先反解出 fx，方形像素 ⇒ fy=fx。
        """
        fx = (self.w / 2.0) / math.tan(math.radians(self.fov) / 2.0)
        return [fx, fx, self.w / 2.0, self.h / 2.0]

    def destroy(self):
        if self.rgb is not None:
            self.rgb.stop()
            self.rgb.destroy()


def carla_tf_to_pose(tf):
    """CARLA transform → docs/69 的 position + quaternion[x,y,z,w]。

    CARLA 用左手系、**度**制欧拉角（roll/pitch/yaw），与 Unity 同为左手系，
    但轴向定义不同（CARLA x=forward, y=right, z=up；Unity x=right, y=up, z=forward）。
    这里按 CARLA 原生轴向出，**不做轴变换**——交付方要的是自洽的位姿+内参，
    做一次轴变换反而多一层出错面（docs/70 辩题二「每省一层转换就少一类打回」的同理）。
    轴向约定写进交付文档。
    """
    loc = tf.location
    r = math.radians(tf.rotation.roll)
    p = math.radians(tf.rotation.pitch)
    y = math.radians(tf.rotation.yaw)
    cy, sy = math.cos(y * 0.5), math.sin(y * 0.5)
    cp, sp = math.cos(p * 0.5), math.sin(p * 0.5)
    cr, sr = math.cos(r * 0.5), math.sin(r * 0.5)
    return ([loc.x, loc.y, loc.z],
            [sr * cp * cy - cr * sp * sy,
             cr * sp * cy + sr * cp * sy,
             cr * cp * sy - sr * sp * cy,
             cr * cp * cy + sr * sp * sy])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--town", default="Town10HD_Opt")
    ap.add_argument("--duration", type=float, default=60.0)
    ap.add_argument("--aspect", default="16:9", choices=list(ASPECT_RES))
    ap.add_argument("--vehicles", type=int, default=60)
    ap.add_argument("--walkers", type=int, default=40)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2000)
    args = ap.parse_args()

    rng = random.Random(args.seed)
    os.makedirs(args.out, exist_ok=True)
    n_frames = int(round(args.duration * FPS))

    client = carla.Client(args.host, args.port)
    client.set_timeout(120.0)
    # **地图已经是目标就不要 load_world**：重载地图是 CARLA 的高风险操作，
    # headless 下实测会把 server 打成 Signal 11（批里每条都 load ⇒ 崩溃概率 ×N），
    # 而且重载本身要几十秒。复用已加载世界既稳又快。
    world = client.get_world()
    if args.town.split("_")[0] not in world.get_map().name:
        print("[carla] loading %s (current: %s)" % (args.town, world.get_map().name))
        world = client.load_world(args.town)
    else:
        print("[carla] reusing loaded map %s" % world.get_map().name)

    # ---- 同步模式：同 tick ⇒ 4 路帧对齐恒 0ms（构造保证，非事后校准）----
    settings = world.get_settings()
    settings.synchronous_mode = True
    settings.fixed_delta_seconds = 1.0 / FPS
    world.apply_settings(settings)

    tm = client.get_trafficmanager()
    tm.set_synchronous_mode(True)
    tm.set_random_device_seed(args.seed)

    bp_lib = world.get_blueprint_library()
    spawn_points = world.get_map().get_spawn_points()
    rng.shuffle(spawn_points)

    actors = []
    lanes = []
    try:
        # ---- 动态物体：车流 + 行人（CARLA 原生，docs/69 载具与人形配额）----
        vehicle_bps = bp_lib.filter("vehicle.*")
        for sp in spawn_points[:args.vehicles]:
            v = world.try_spawn_actor(rng.choice(vehicle_bps), sp)
            if v:
                v.set_autopilot(True, tm.get_port())
                actors.append(v)
        walker_bps = bp_lib.filter("walker.pedestrian.*")
        for _ in range(args.walkers):
            loc = world.get_random_location_from_navigation()
            if loc is None:
                continue
            w = world.try_spawn_actor(rng.choice(walker_bps),
                                      carla.Transform(loc))
            if not w:
                continue
            actors.append(w)
            ctrl = world.try_spawn_actor(bp_lib.find("controller.ai.walker"),
                                         carla.Transform(), attach_to=w)
            if ctrl:
                actors.append(ctrl)
                ctrl.start()
                ctrl.go_to_location(world.get_random_location_from_navigation())
        print("[carla] spawned %d dynamic actors" % len(actors))

        # ---- 4 路 rig：焦距各 25%（一条内四路各占一档，docs/69）----
        #
        # **相机 attach 到载具，不每帧 set_transform**。首版让 4 个自由飞的
        # sensor 每帧 set_transform，实测跑 200 帧上下就有 lane 停止投递、
        # queue.get 超时——CARLA 对 sensor 频繁改 transform 会把它的内部状态
        # 打乱。官方的多相机用法一律是 attach 到 actor、移动 actor。
        # 这同时贴合 docs/69 的载具视角类目：轨迹由车流自然产生，
        # 物理约束（速度/转向/不穿模）全部免费。
        focals = list(FOCALS)
        rng.shuffle(focals)
        carriers = [a for a in actors if a.type_id.startswith("vehicle.")][:LANES]
        if len(carriers) < LANES:
            raise RuntimeError("需要至少 %d 辆载机车，实际 %d" % (LANES, len(carriers)))
        # 四路挂同一批车但偏移不同：前视 / 左后 45° / 右后 45° / 高位俯瞰，
        # 保证有共视又不完全同构（解耦）
        rigs = [(carla.Location(x=1.6, z=1.6), carla.Rotation()),
                (carla.Location(x=-2.2, y=-1.2, z=1.8), carla.Rotation(yaw=-35)),
                (carla.Location(x=-2.2, y=1.2, z=1.8), carla.Rotation(yaw=35)),
                (carla.Location(x=-5.0, z=3.2), carla.Rotation(pitch=-12))]
        for i in range(LANES):
            lane = LaneRig(world, bp_lib, i, focals[i], args.aspect, args.out)
            loc, rot = rigs[i]
            lane.spawn(world, carla.Transform(loc, rot), parent=carriers[i])
            lanes.append(lane)
        print("[carla] 4 lanes up, focals=%s aspect=%s" % (focals, args.aspect))

        # ---- 采集：每 tick 推进 + 收 4 路 RGB/depth + 记位姿 ----
        for f in range(n_frames):
            # 轨迹由载具的 autopilot 行驶产生，相机随车走——不再每帧
            # set_transform（见 rig 组装处的说明）
            tick_id = world.tick()

            for lane in lanes:
                # **按 tick id 对齐**：同步模式下传感器回调是异步投递的，
                # 队列里可能还压着上一 tick 的帧；直接 get 会错位，且一旦
                # 某 tick 的帧迟到就再也追不上，最终 queue.Empty 超时
                # （实测首版就是这样在中途挂掉）。丢弃早于本 tick 的帧，
                # 等到 frame == tick_id 那一张才算数。
                rgb = None
                for _ in range(8):
                    img = lane.rgb_q.get(timeout=30.0)
                    if img.frame >= tick_id:
                        rgb = img
                        break
                if rgb is None:
                    raise RuntimeError("lane %d never delivered frame %d"
                                       % (lane.idx + 1, tick_id))
                lane.write_frame(rgb)
                pos, quat = carla_tf_to_pose(lane.rgb.get_transform())
                # 相机是自由飞的传感器、没有物理速度，用逐帧位置差分（× FPS）——
                # 与 Unity 侧 PoseRecorder 的 camera_speed 口径一致
                if lane.frames:
                    q = lane.frames[-1]["camera_position"]
                    speed = math.dist(pos, q) * FPS
                else:
                    speed = 0.0
                lane.frames.append({
                    "frame": f,
                    "fps": float(FPS),
                    "camera_position": pos,
                    "camera_rotation_quaternion": quat,
                    "camera_intrinsics": lane.intrinsics(),
                    "camera_speed": speed,
                })
            if f % 150 == 0:
                print("[carla] frame %d/%d" % (f, n_frames), flush=True)

        # ---- 落盘：先收 ffmpeg 管道，再写 json ----
        for lane in lanes:
            lane.finish()
        for lane in lanes:
            with open(os.path.join(args.out, "action_camera_%d.json" % (lane.idx + 1)),
                      "w", encoding="utf-8") as fh:
                json.dump(lane.frames, fh, ensure_ascii=False)
            with open(os.path.join(args.out, "systeminfo_%d.json" % (lane.idx + 1)),
                      "w", encoding="utf-8") as fh:
                json.dump({"gameProcessName": "CarlaUE4",
                           "screenWidth": lane.w, "screenHeight": lane.h,
                           "gameWidth": lane.w, "gameHeight": lane.h,
                           "recordDpi": 1.0}, fh)
        with open(os.path.join(args.out, "provenance.json"), "w", encoding="utf-8") as fh:
            json.dump({
                "scene_asset_id": "carla-%s@0.9.15" % args.town,
                "license_id": "cc-by",
                "license": "CC-BY (CARLA specific assets) + MIT (CARLA code)",
                "attribution": "Scenes and assets from the CARLA simulator "
                               "(https://carla.org), CC-BY licensed.",
                "seed": args.seed,
                "notes": [
                    {"k": "axis_convention",
                     "v": "CARLA native left-handed: x=forward, y=right, z=up; "
                          "rotations are CARLA roll/pitch/yaw in degrees, "
                          "quaternion emitted as [x,y,z,w]. No axis remap applied."},
                    {"k": "deliverables",
                     "v": "MC per docs/69 = 12 files (video/action_camera/systeminfo x4); "
                          "no depth in this bundle — depth belongs to the SFM order (docs/68)"},
                    {"k": "sync",
                     "v": "synchronous_mode + fixed_delta_seconds=1/30; all 4 lanes "
                          "sample on the same tick, so inter-lane alignment is 0ms "
                          "by construction"},
                ],
            }, fh, ensure_ascii=False, indent=2)
        print("[carla] DONE frames=%d out=%s" % (n_frames, args.out))

    finally:
        for lane in lanes:
            lane.destroy()
        # 批量销毁：`apply_batch` 是 CARLA 官方的清理方式，对已死 actor 不抛异常。
        # 逐个 destroy 不行——CARLA 会自行回收走完路径的 walker controller，
        # 对已死 actor 调 destroy 抛的是 **C++ std::runtime_error**，
        # Python 的 except RuntimeError 接不住，进程直接 terminate，
        # 把真正的异常一起吃掉（首版排查绕了一圈就栽在这）。
        for a in actors:
            try:
                if a.type_id.startswith("controller."):
                    a.stop()
            except (RuntimeError, AttributeError):
                pass
        try:
            client.apply_batch([carla.command.DestroyActor(a) for a in actors])
        except RuntimeError:
            pass

        s = world.get_settings()
        s.synchronous_mode = False
        s.fixed_delta_seconds = None
        world.apply_settings(s)


if __name__ == "__main__":
    main()
