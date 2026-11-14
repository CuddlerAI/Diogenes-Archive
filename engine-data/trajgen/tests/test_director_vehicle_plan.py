"""vehicle_plan.json 的坐标系契约（0806 双重世界偏移缺陷回归）。

缺陷：v1.0 把场景快照的 `world_offset` 烘进了 `frames_world`，而采集侧
`CarrierVehicleSpawner` 按「生成器局部系」又加了一遍运行时偏移 ⇒ 车体被平移两次。
s18 实证：车体离本路相机 114.36m（该距离本应恒为 |mount_offset|=3.33m），
统览画面里三辆载机全部不在场，而位姿断言只管相机、可见性自检只核 cullingMask、
dyn.active_frac 由环境车流撑满 ⇒ 三道自检同时失明。

守的不变量：`vehicle_plan.json` 与 `action_camera_*.json` **同系**——
相机 = 车体位姿 ∘ mount_offset（`mount_world`），逐帧成立。任何一侧再叠一次
世界偏移，这条等式立刻差出 |world_offset| 的量级（s18 为 115m）。
采集侧的同名断言在 `CarrierVehicleSpawner.VerifyMount`（超差即 AbortClip）。
"""

import json
import math
from pathlib import Path

import pytest

from trajgen.director import MOUNT_OFFSET, mount_world, plan_director_clip

#: 真实占据快照（SceneProbeExporter 产物）——导演模式只认它，合成场景无 anchor_corridors
SCENE_FILE = (Path(__file__).resolve().parents[2]
              / "capture-orchestrator" / "megacity-scene.json")
#: 缺陷取证用的种子/场景组合（s18 打头）。计划闸门与本用例无关，过不去就换下一个：
#: 这里要验的是「写出来的坐标在哪个系」，不是单种子过率（那条闸在 test_plan_export）。
#: 多备几个是因为过闸与否随 director 调参版本漂移（同一场景 seed 18 在有的版本上排不出）。
SEEDS = (18, 19, 20, 13)


@pytest.fixture(scope="module")
def plan_dir(tmp_path_factory):
    if not SCENE_FILE.is_file():
        pytest.skip("缺场景快照：%s" % SCENE_FILE)
    last = ""
    for seed in SEEDS:
        out = tmp_path_factory.mktemp("director-%d" % seed)
        try:
            plan_director_clip(str(SCENE_FILE), seed, out, duration_s=60.0,
                               n_extras=2, covis_hz=2.0)
        except RuntimeError as e:      # 8 次重排耗尽：与本契约无关
            last = str(e)
            continue
        return out
    pytest.fail("种子 %s 都没排出计划（%s）——夹具需重钉" % (list(SEEDS), last))


@pytest.fixture(scope="module")
def vehicle_plan(plan_dir):
    return json.loads((plan_dir / "vehicle_plan.json").read_text(encoding="utf-8"))


def _frames(v):
    """取逐帧位姿，字段名不挑（v1.1 `frames_local` / v1.0 `frames_world`）。

    刻意不挑名字：数值用例要在**旧写法**上因为坐标错位而红，
    而不是因为「字段名变了」而红——字段名由 test_frames_are_generator_local 单管。
    """
    return v.get("frames_local") or v["frames_world"]


def test_frames_are_generator_local_not_world(vehicle_plan):
    """字段名与版本随语义走：v1.1 落 frames_local，且不得再出现 frames_world。

    旧名留在文件里最危险——采集侧按局部系读、名字却说是世界系，
    两边各自「照契约办事」就能拼出一个静默错位。
    """
    assert vehicle_plan["version"] == "1.1"
    assert vehicle_plan["frames_frame"] == "generator-local"
    for v in vehicle_plan["vehicles"]:
        assert "frames_local" in v, v["role"]
        assert "frames_world" not in v, "v1.0 字段名复活了：%s" % v["role"]


def test_world_offset_is_metadata_only(vehicle_plan):
    """`world_offset` 只作溯源元数据，**不得**烘进坐标。

    判据不看数值大小，只看契约：把它从坐标里减掉不该改变任何东西 ——
    等价于「车体-相机」等式在不减的情况下已经成立（下一条用例逐帧核）。
    """
    scene = json.loads(SCENE_FILE.read_text(encoding="utf-8"))
    off = scene["world_offset"]
    assert vehicle_plan["world_offset"] == off
    assert max(abs(x) for x in off) > 1.0, "场景偏移退化成 0 就验不出双重平移了"
    assert vehicle_plan["mount_offset"] == list(MOUNT_OFFSET)


def test_carrier_body_and_its_camera_share_one_frame(plan_dir, vehicle_plan):
    """逐帧：mount_world(车体位姿) == 该路相机位姿（两份文件同系的充要证据）。"""
    for v in vehicle_plan["vehicles"]:
        if not str(v["role"]).startswith("carrier"):
            continue
        cam = json.loads((plan_dir / ("action_camera_%d.json" % v["id"]))
                         .read_text(encoding="utf-8"))
        frames = _frames(v)
        assert len(frames) == len(cam) == vehicle_plan["frame_count"]
        worst = max(math.dist(mount_world(tuple(f[:3]), f[3]),
                              tuple(c["camera_position"]))
                    for f, c in zip(frames, cam))
        # 坐标只量化到 1mm ⇒ 容差 1e-2；双重平移会差出 |world_offset| 的量级
        assert worst < 1e-2, "carrier %d 与 cam%d 不同系：最差 %.3f m" % (
            v["id"], v["id"], worst)


def test_body_to_camera_distance_is_the_mount_arm(plan_dir, vehicle_plan):
    """采集侧自检的同一条判据：|车体 - 本路相机| 恒 = |mount_offset|。

    这是 `CarrierVehicleSpawner.VerifyMount` 在引擎里逐 tick 核的量（超差 AbortClip）；
    它与摆位偏移/摆位 yaw 无关（刚体变换保距），所以生成侧能提前把它钉死。
    """
    arm = math.dist((0.0, 0.0, 0.0), MOUNT_OFFSET)
    for v in vehicle_plan["vehicles"]:
        if not str(v["role"]).startswith("carrier"):
            continue
        cam = json.loads((plan_dir / ("action_camera_%d.json" % v["id"]))
                         .read_text(encoding="utf-8"))
        worst = max(abs(math.dist(tuple(f[:3]), tuple(c["camera_position"])) - arm)
                    for f, c in zip(_frames(v), cam))
        assert worst < 0.25, "carrier %d 挂载臂长偏差 %.3f m（引擎侧同判据容差 0.25）" % (
            v["id"], worst)
