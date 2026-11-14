"""端到端：生成一条计划 → 导出次级文件夹 → 逐条核对交付契约。

这里的断言就是 engine-data-qc `check-clip --mode mc` 会查的那些，
两侧同时绿才算联测通过（QC 侧独立复核见 `engine-data/qc/tests`）。
"""

import json
import math

from trajgen import metrics as met
from trajgen.cam4 import V4_CAP
from trajgen.export import frame_records, systeminfo, write_clip
from trajgen.planner import (ASPECT_RES, DURATION_S, FPS, N_FRAMES, QuotaTracker,
                             generate, radius_band)
from trajgen.scene import synthetic

BLUE = ["time", "fps", "frame", "camera_position", "camera_rotation_quaternion",
        "camera_Follow Offset", "camera_intrinsics", "camera_speed"]
WHITE = ["camera_rotation_oula", "player_position", "player_rotation_oula",
         "player_rotation_quaternion", "player_speed", "mouse_x", "mouse_y",
         "mouse_dx", "mouse_dy", "keyCode", "metric_scale"]


def _plan(seed=1):
    """夹具计划：**钉死 seed=1**，每次重新生成。

    为什么是 1 不是 0（2026-09-05）：第四路粉色收口（#904）之后，seed 0 在 macOS libm
    上第 2 次 attempt 过闸，在 glibc（CI 的 ubuntu）上 7 次 attempt 全挂——同一串随机数、
    同一份代码，差异来自 libm 的 1-ulp 级浮点差在自由壳投影 / 排窗的分支处被放大
    （实测同一候选在两平台上 max_push 21.0 m vs 3.0 m、clearance +0.50 m vs −3.09 m）。
    单种子过率两平台都是 9–10/10，问题只在「哪一个种子」。seed 1 两平台**一次过闸**且
    余量最大（covis 0.70 / clearance 0.60 m / push 0.15 m，逐位相同），故钉它。
    这不是搜种子：不通过就该红的纪律不变，「过率」仍由 test_single_seed_pass_rate 盯。

    共视改严读（#462）之后这条一度红了，当时的处置是把夹具改成「顺序搜前 60 个
    种子里第一条过闸的」。那样确实能变绿，但它把「单种子过率只有两成」这件事
    一起藏了起来 —— 用例照绿，M3a 排程仿真却是 12.5%。经典管线按严口径重调后
    seed 0 自己就过（covis 0.554），夹具随之钉回去；「过率」由
    `test_single_seed_pass_rate` 直接盯，不再借夹具间接反映。

    不缓存计划对象：`test_plan_is_deterministic` 要的就是「同种子必同」，
    缓存会让那条用例退化成自比自。
    """
    return generate(synthetic("open_plaza", 1), seed=seed, quota=QuotaTracker())


def test_plan_is_deterministic():
    """同种子必同 —— 「种子入 provenance 可复现」（docs/70 §4.2）。"""
    a = _plan()
    b = _plan()
    assert a.tracks[0].positions[:20] == b.tracks[0].positions[:20]
    assert a.cam4_plan.to_dict() == b.cam4_plan.to_dict()


def test_blue_white_field_split():
    """蓝=每相机必含、白=仅 cam1（0803 实测判定，docs/71 §8）。"""
    plan = _plan()
    main = frame_records(plan, 0)[0]
    other = frame_records(plan, 1)[0]
    for k in BLUE:
        assert k in main and k in other, k
    for k in WHITE:
        assert k in main, k
        assert k not in other, k


def test_key_name_quirks_follow_fixtures():
    """键名以需求方样例为准，勿「修正」：MC 是 'camera_Follow Offset' + mouse_x。"""
    rec = frame_records(_plan(), 0)[0]
    assert "camera_Follow Offset" in rec
    assert "camera_follow_offset" not in rec        # 68 的小写键不得混入 MC
    assert "mouseX" not in rec and "mouseY" not in rec
    assert "mouse_x" in rec and "mouse_y" in rec


def test_frame_axis_and_duration():
    recs = frame_records(_plan(), 0)
    assert len(recs) == N_FRAMES == int(DURATION_S * FPS)
    assert [r["frame"] for r in recs[:5]] == [0, 1, 2, 3, 4]
    assert recs[-1]["frame"] == N_FRAMES - 1
    assert all(r["fps"] == FPS for r in recs[::200])


def test_intrinsics_constant_and_square_pixels():
    """FOV 恒定是 8.1 红线；fx=fy 由方形像素保证。"""
    plan = _plan()
    for idx in range(4):
        recs = frame_records(plan, idx)
        first = recs[0]["camera_intrinsics"]
        assert first["fx"] == first["fy"]
        assert all(r["camera_intrinsics"] == first for r in recs[::150])


def test_quaternions_are_unit_at_delivery_precision():
    for idx in range(4):
        for r in frame_records(_plan(), idx)[::150]:
            q = r["camera_rotation_quaternion"]
            assert abs(math.sqrt(sum(x * x for x in q)) - 1.0) < 1e-3


def test_aspect_labels_match_capture_side_verbatim():
    """画幅标签 → 像素尺寸必须与采集侧 `CaptureFormats.cs` 逐字一致。

    需求 §2 的措辞是「横屏 3:4」「竖屏 4:3」——与常规 W:H 记法相反，但两侧都得照抄。
    这里**字面钉死**而不是拿 `ASPECT_RES` 自比自：原来的用例是
    `systeminfo(plan)` 对 `ASPECT_RES`，两边同源，规划侧把 3:4/4:3 写反了也照样绿，
    结果交付文件（采集侧写的）是对的、而规划侧拿错画幅去算内参和共视，
    4:3 那条 fx 差 33%。
    """
    assert ASPECT_RES["16:9"] == (1920, 1080)
    assert ASPECT_RES["9:16"] == (1080, 1920)
    assert ASPECT_RES["3:4"] == (1440, 1080), "3:4 是横屏（需求逐字）"
    assert ASPECT_RES["4:3"] == (1080, 1440), "4:3 是竖屏（需求逐字）"


def test_systeminfo_resolution_matches_aspect():
    plan = _plan()
    si = systeminfo(plan)
    assert (si["x"], si["y"]) == (0, 0)
    assert (si["width"], si["height"]) == ASPECT_RES[plan.aspect]
    assert si["recordDpi"] == 1.0


def test_single_seed_pass_rate():
    """**单种子过率**闸：前 8 个种子至少 6 条一次过闸。

    这条盯的是上一轮真正漏掉的东西。共视改严读后单种子过率掉到两成，而全部
    既有用例只问「一条合格计划长什么样」，把夹具换成「搜到过闸的那个种子」
    就能全绿 —— 于是 M3a 排程仿真 12.5% 这件事在测试里没有任何投影。
    过率是排程仿真的验收口径（docs/71 §10 ≥80%），它应当有一条直接的守门。
    """
    ok = [generate(synthetic("open_plaza", 1), seed=s, quota=QuotaTracker()).ok
          for s in range(8)]
    assert sum(ok) >= 6, "8 个种子只过 %d 条：单种子过率回归了 %s" % (sum(ok), ok)


def test_gates_hold_on_a_passing_plan():
    plan = _plan()
    assert plan.ok, plan.reasons
    dt = 1.0 / FPS
    for tr in plan.tracks:
        ph = met.physics(tr.positions, tr.quats, dt)
        assert not ph.violations(is_cam4=(tr.cam_id == 4)), (tr.cam_id, ph.to_dict())
    assert plan.decouple.ok()
    assert plan.covis.ratio >= 0.5
    assert plan.init_angle <= 15.0 and plan.init_dist <= 30.0
    assert plan.min_clearance_m > 0.0


def test_cam4_rho_is_recoverable_by_qc_algorithm():
    """QC 会从 v₄/v_g 反算 ρ —— 申报值与反算值必须对得上。"""
    plan = _plan()
    gi = plan.cam4_plan.guide_cam - 1
    chk = met.cam4_check(plan.tracks[3].positions, plan.tracks[gi].positions, 1.0 / FPS)
    assert not chk.problems(plan.cam4_plan.t_sw, plan.cam4_plan.delta), chk
    assert len(chk.events_s) == 1
    assert 20.0 <= plan.cam4_plan.t_sw <= 40.0
    assert max(met.physics(plan.tracks[3].positions, plan.tracks[3].quats,
                           1.0 / FPS).speed_max, 0.0) <= V4_CAP


def test_rho_bands_avoid_forbidden_zone():
    """ρ 取自 [0.33,0.60]∪[1.65,3.0]，禁带 0.65–1.5 只穿越不驻留。"""
    cp = _plan().cam4_plan
    for r in (cp.rho1, cp.rho2):
        assert not (0.65 < r < 1.5), r


def test_write_clip_emits_full_json_pack(tmp_path):
    plan = _plan()
    out = write_clip(plan, tmp_path / "clip")
    for n in range(1, 5):
        assert (out / ("action_camera_%d.json" % n)).is_file()
        assert (out / ("systeminfo_%d.json" % n)).is_file()
    covis = json.loads((out / "covis.json").read_text(encoding="utf-8"))
    assert covis["covis_ratio"] >= 0.5
    plan_json = json.loads((out / "plan.json").read_text(encoding="utf-8"))
    assert plan_json["mode"] == "mc" and plan_json["duration_s"] == 60.0
    assert len(plan_json["cameras"]) == 4
    assert sorted(c["focal_mm"] for c in plan_json["cameras"]) == [18, 24, 35, 50]
    assert plan_json["cam4"]["guide_cam"] in (1, 2, 3)
    prov = json.loads((out / "provenance.json").read_text(encoding="utf-8"))
    assert prov["scene"]["synthetic"] is True


def test_radius_band_matches_footprint_scale():
    """r/f 恒定 ⇒ 各路画面足迹尺度一致（min 口径共视能过的前提）。"""
    ratios = [radius_band(f)[0] / f for f in (18, 24, 35, 50)]   # 下界未被夹持
    assert max(ratios) - min(ratios) < 1e-9


def test_speed_fields_match_capture_side_convention():
    """`*_speed` 的差分口径必须与采集侧 `PoseRecorder` 逐字一致。

    采集侧写的是 `speed[0]=0`、`speed[k]=(pos[k]−pos[k−1])·fps`（**后向**差分），
    `engine_qc` 的 `num.player_speed` 也按这个口径核。规划侧此前写前向差分、
    且把 `player_speed` 的竖直分量硬写 0（而 `player_position` 的 y 跟着相机走），
    于是 `gen-clip` 出的每一条都挂 `num.player_speed`。这条守的是跨侧口径本身。
    """
    plan = _plan()
    recs = frame_records(plan, 0)
    dt = 1.0 / FPS
    assert recs[0]["camera_speed"] == [0.0, 0.0, 0.0]
    assert recs[0]["player_speed"] == [0.0, 0.0, 0.0]
    for k in (1, 137, 900, N_FRAMES - 1):
        for field, pos in (("camera_speed", "camera_position"),
                           ("player_speed", "player_position")):
            back = [(recs[k][pos][j] - recs[k - 1][pos][j]) / dt for j in range(3)]
            for j in range(3):
                assert abs(recs[k][field][j] - back[j]) < 1e-3, (k, field, j)
